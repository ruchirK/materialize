// Copyright Materialize, Inc. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use byteorder::{NetworkEndian, ReadBytesExt};
use lazy_static::lazy_static;
use log::error;
use regex::Regex;
use tokio::select;
use tokio::sync::mpsc;

use dataflow_types::Update;
use expr::GlobalId;
use repr::{Row, Timestamp};

use crate::wal::{encode_progress, encode_update};

// TODO: change this to be a longer time interval. Keeping it short for tests.
// TODO: Lets add some jitter to compaction so we aren't compacting every single
// table at the same time maybe?
static COMPACTER_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub enum CompacterMessage {
    Add(GlobalId),
    Drop(GlobalId),
    Resume(GlobalId, Trace),
    AllowCompaction(Timestamp),
}

#[derive(Debug)]
struct Batch {
    upper: Timestamp,
    lower: Timestamp,
    path: PathBuf,
}

impl Batch {
    fn create(log_segment_path: &Path, trace_path: &Path) -> Result<Self, anyhow::Error> {
        let messages = read_segment(log_segment_path)?;
        Batch::create_from_messages(messages, trace_path, None)
    }

    fn create_from_messages(
        messages: Vec<Message>,
        trace_path: &Path,
        compaction_frontier: Option<Timestamp>,
    ) -> Result<Self, anyhow::Error> {
        let mut upper: Option<Timestamp> = None;
        let mut lower: Option<Timestamp> = None;
        let mut time_data = BTreeMap::new();
        for message in messages.iter() {
            match message {
                Message::Progress(time) => match (lower, upper) {
                    (None, None) => {
                        lower = Some(*time);
                    }
                    (Some(l), None) => {
                        assert!(*time >= l);
                        upper = Some(*time);
                    }
                    (Some(_), Some(u)) => {
                        assert!(*time >= u);
                        upper = Some(*time);
                    }
                    (None, Some(_)) => unreachable!(),
                },
                Message::Data(Update {
                    row,
                    timestamp,
                    diff,
                }) => {
                    let time = if let Some(frontier) = compaction_frontier {
                        std::cmp::min(frontier, *timestamp)
                    } else {
                        *timestamp
                    };
                    let entry = time_data.entry((time, row)).or_insert(0);
                    *entry += diff;
                }
            }
        }

        // Now let's prepare the output
        let mut buf = Vec::new();
        assert!(lower.is_some());
        assert!(upper.is_some());

        let lower = lower.unwrap();
        let upper = upper.unwrap();

        encode_progress(lower, &mut buf)?;
        for ((timestamp, row), diff) in time_data.into_iter() {
            if diff == 0 {
                continue;
            }

            encode_update(row, timestamp, diff, &mut buf)?;
        }

        encode_progress(upper, &mut buf)?;

        let batch_name = format!("batch-{}-{}", lower, upper);
        let batch_path = trace_path.join(&batch_name);
        let batch_tmp_path = trace_path.join(format!("{}-tmp", batch_name));
        fs::write(&batch_tmp_path, buf)?;
        fs::rename(&batch_tmp_path, &batch_path)?;

        Ok(Batch {
            upper,
            lower,
            path: batch_path,
        })
    }

    fn reinit(path: PathBuf) -> Result<Self, anyhow::Error> {
        let batch_name = path
            .file_name()
            .expect("batch name known to exist")
            .to_str()
            .expect("batch name known to be valid utf8");
        let parts: Vec<_> = batch_name.split('-').collect();
        assert!(parts.len() == 3);
        Ok(Self {
            upper: parts[2].parse()?,
            lower: parts[1].parse()?,
            path,
        })
    }

    fn read(&self) -> Result<Vec<Message>, anyhow::Error> {
        read_segment(&self.path)
    }

    fn compact(
        batches: &[Batch],
        trace_path: &Path,
        compaction_frontier: Option<Timestamp>,
    ) -> Result<Self, anyhow::Error> {
        let mut messages = vec![];

        for batch in batches {
            messages.append(&mut read_segment(&batch.path)?);
        }

        Batch::create_from_messages(messages, trace_path, compaction_frontier)
    }
}

#[derive(Debug)]
pub struct Trace {
    trace_path: PathBuf,
    wal_path: PathBuf,
    batches: Vec<Batch>,
    compaction: Option<Timestamp>,
}

impl Trace {
    fn create(id: GlobalId, trace_path: PathBuf, wal_path: PathBuf) -> Result<Self, anyhow::Error> {
        let _ = fs::read_dir(&wal_path).with_context(|| {
            anyhow!(
                "trying to ensure wal directory {} exists for trace of relation {}",
                id,
                wal_path.display()
            )
        })?;

        // Create a new directory to store the trace
        fs::create_dir(&trace_path).with_context(|| {
            anyhow!("trying to create trace directory: {}", trace_path.display())
        })?;

        Ok(Self {
            trace_path,
            wal_path,
            batches: Vec::new(),
            compaction: None,
        })
    }

    // Let's delete the trace directory and the WAL directory.
    fn destroy(self) -> Result<(), anyhow::Error> {
        fs::remove_dir_all(self.trace_path)?;
        fs::remove_dir_all(self.wal_path)?;

        Ok(())
    }

    // Try to consume more completed log segments from the wal directory
    fn consume_wal(&mut self) -> Result<(), anyhow::Error> {
        let finished_segments = self.find_finished_wal_segments()?;

        for segment in finished_segments {
            let batch = Batch::create(&segment, &self.trace_path)?;
            self.batches.push(batch);
            fs::remove_file(&segment)?;
        }

        Ok(())
    }

    fn find_finished_wal_segments(&self) -> Result<Vec<PathBuf>, anyhow::Error> {
        lazy_static! {
            static ref FINISHED_WAL_SEGMENT_REGEX: Regex =
                Regex::new("^log-[0-9]+-final$").unwrap();
        }

        let mut segments = read_dir_regex(&self.wal_path, &FINISHED_WAL_SEGMENT_REGEX)?;
        segments.sort();

        Ok(segments)
    }

    fn find_unfinished_wal_segment(&self) -> Result<PathBuf, anyhow::Error> {
        lazy_static! {
            static ref UNFINISHED_WAL_SEGMENT_REGEX: Regex = Regex::new("^log-[0-9]+$").unwrap();
        }

        let mut segments = read_dir_regex(&self.wal_path, &UNFINISHED_WAL_SEGMENT_REGEX)?;
        if segments.len() > 1 {
            bail!(
                "Expected only a single unfinished wal segment at {}. Found {}",
                self.wal_path.display(),
                segments.len()
            );
        }

        if segments.len() == 0 {
            bail!(
                "Expected at least a single unfinished wal segment at {}. Found none.",
                self.wal_path.display()
            );
        }

        Ok(segments.pop().unwrap())
    }

    fn find_batches(&self) -> Result<Vec<Batch>, anyhow::Error> {
        lazy_static! {
            static ref BATCH_REGEX: Regex = Regex::new("^batch-[0-9]+-[0-9]+$").unwrap();
        }

        let mut batches = read_dir_regex(&self.trace_path, &BATCH_REGEX)?;
        batches.sort();

        let batches: Vec<Batch> = batches
            .into_iter()
            .map(Batch::reinit)
            .collect::<Result<_, _>>()
            .unwrap();
        Ok(batches)
    }

    // Try to compact all of the batches we know about into a single batch from
    // [compaction_frontier, upper)
    fn compact(&mut self) -> Result<(), anyhow::Error> {
        self.consume_wal()?;

        if self.batches.len() > 10 {
            let batches = std::mem::replace(&mut self.batches, vec![]);
            let batch = Batch::compact(&batches, &self.trace_path, self.compaction)?;
            self.batches.push(batch);

            // TODO: This seems like potentially a place with a weird failure mode.
            for batch in batches {
                fs::remove_file(batch.path)?;
            }
        }

        Ok(())
    }

    pub fn resume(
        id: GlobalId,
        traces_path: PathBuf,
        wals_path: PathBuf,
    ) -> Result<Self, anyhow::Error> {
        // Need to instantiate a new trace and figure out what batches
        // we have access to.
        let trace_path = traces_path.join(id.to_string());
        let wal_path = wals_path.join(id.to_string());
        let mut ret = Self {
            trace_path,
            wal_path,
            batches: Vec::new(),
            compaction: None,
        };

        let mut batches = ret.find_batches()?;
        ret.batches.append(&mut batches);

        Ok(ret)
    }

    pub fn read(&self) -> Result<Vec<Message>, anyhow::Error> {
        let mut out = vec![];

        for batch in self.batches.iter() {
            let mut messages = batch.read()?;
            out.append(&mut messages);
        }

        let finished_segments = self.find_finished_wal_segments()?;
        let unfinished_segment = self.find_unfinished_wal_segment()?;

        for segment in finished_segments {
            let mut messages = read_segment(&segment)?;
            out.append(&mut messages);
        }

        out.append(&mut read_segment(&unfinished_segment)?);

        out.dedup();
        Ok(out)
    }
}

pub struct Compacter {
    rx: mpsc::UnboundedReceiver<CompacterMessage>,
    traces: HashMap<GlobalId, Trace>,
    traces_path: PathBuf,
    wals_path: PathBuf,
}

impl Compacter {
    pub fn new(
        rx: mpsc::UnboundedReceiver<CompacterMessage>,
        traces_path: PathBuf,
        wals_path: PathBuf,
    ) -> Result<Self, anyhow::Error> {
        fs::create_dir_all(&traces_path).with_context(|| {
            anyhow!(
                "trying to create traces directory: {}",
                traces_path.display()
            )
        })?;
        Ok(Self {
            rx,
            traces: HashMap::new(),
            traces_path,
            wals_path,
        })
    }

    async fn compact(&mut self) -> Result<(), anyhow::Error> {
        let mut interval = tokio::time::interval(COMPACTER_INTERVAL);
        loop {
            select! {
                data = self.rx.recv() => {
                    if let Some(data) = data {
                        self.handle_message(data)?
                    } else {
                        break;
                    }
                }
                _ = interval.tick() => {
                    for (_, trace) in self.traces.iter_mut() {
                        // Check to see if the WAL still exists
                        // if so, check to see if there are any pending log segments to ingest
                        // finally, check to see if we can compact the data.
                        trace.compact()?;
                    }
                }
            }
        }

        Ok(())
    }

    fn handle_message(&mut self, message: CompacterMessage) -> Result<(), anyhow::Error> {
        match message {
            CompacterMessage::Add(id) => {
                if self.traces.contains_key(&id) {
                    bail!(
                        "asked to create trace for relation {} which already exists.",
                        id
                    );
                }
                let trace_path = self.traces_path.join(id.to_string());
                let wal_path = self.wals_path.join(id.to_string());

                let trace = Trace::create(id, trace_path, wal_path)?;
                self.traces.insert(id, trace);
            }
            CompacterMessage::Drop(id) => {
                if !self.traces.contains_key(&id) {
                    bail!(
                        "asked to drop trace for relation {} which doesn't exist.",
                        id
                    );
                }

                let trace = self.traces.remove(&id).expect("trace known to exist");
                trace.destroy()?;
            }
            CompacterMessage::Resume(id, trace) => {
                if self.traces.contains_key(&id) {
                    bail!(
                        "asked to resume trace for relation {} which already exists.",
                        id
                    );
                }
                self.traces.insert(id, trace);
            }
            CompacterMessage::AllowCompaction(frontier) => {
                for (_, trace) in self.traces.iter_mut() {
                    if let Some(compaction_frontier) = trace.compaction {
                        assert!(frontier >= compaction_frontier);
                    }

                    trace.compaction = Some(frontier);
                }
            }
        };
        Ok(())
    }

    pub async fn run(&mut self) {
        let ret = self.compact().await;

        match ret {
            Ok(_) => (),
            Err(e) => {
                error!("Compacter thread encountered an error: {:#}", e);
            }
        }
    }
}

fn read_dir_regex(path: &Path, regex: &Regex) -> Result<Vec<PathBuf>, anyhow::Error> {
    let entries = std::fs::read_dir(path)?;
    let mut results = vec![];
    for entry in entries {
        if let Ok(file) = entry {
            let path = file.path();
            let file_name = path.file_name();
            if file_name.is_none() {
                continue;
            }

            let file_name = file_name.unwrap().to_str();

            if file_name.is_none() {
                continue;
            }

            let file_name = file_name.unwrap();
            if regex.is_match(&file_name) {
                results.push(path.to_path_buf());
            }
        }
    }

    Ok(results)
}

#[derive(Debug, PartialEq)]
pub enum Message {
    Data(Update),
    Progress(Timestamp),
}

fn read_message(buf: &[u8], mut offset: usize) -> Option<(Message, usize)> {
    if offset >= buf.len() {
        return None;
    }

    // Let's start by only looking at the buffer at the offset.
    let (_, data) = buf.split_at(offset);

    // Let's read the header first
    let mut cursor = Cursor::new(&data);

    let is_progress = cursor.read_u32::<NetworkEndian>().unwrap();

    if is_progress != 0 {
        // If this is a progress message let's seal a new
        // set of updates.

        // Lets figure out the time bound.
        let timestamp = cursor.read_u64::<NetworkEndian>().unwrap();
        // Advance the offset past what we've read.
        offset += 12;

        Some((Message::Progress(timestamp), offset))
    } else {
        // Otherwise lets read the data.
        let timestamp = cursor.read_u64::<NetworkEndian>().unwrap();
        let diff = cursor.read_i64::<NetworkEndian>().unwrap() as isize;
        let len = cursor.read_u32::<NetworkEndian>().unwrap() as usize;

        assert!(diff != 0);

        // Grab the next len bytes after the 24 byte length header, and turn
        // it into a vector so that we can extract things from it as a Row.
        // TODO: could we avoid the extra allocation here?
        let (_, rest) = data.split_at(24);
        let row = rest[..len].to_vec();

        let row = unsafe { Row::new(row) };
        // Update the offset to account for the data we just read
        offset = offset + 24 + len;
        Some((
            Message::Data(Update {
                row,
                timestamp,
                diff,
            }),
            offset,
        ))
    }
}

/// Iterator through a set of persisted messages.
#[derive(Debug)]
pub struct LogSegmentIter {
    /// Underlying data from which we read the records.
    pub data: Vec<u8>,
    /// Offset into the data.
    pub offset: usize,
}

impl Iterator for LogSegmentIter {
    type Item = Message;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some((message, next_offset)) = read_message(&self.data, self.offset) {
            self.offset = next_offset;
            Some(message)
        } else {
            None
        }
    }
}

impl LogSegmentIter {
    pub fn new(data: Vec<u8>) -> Self {
        Self { data, offset: 0 }
    }
}

fn read_segment(path: &Path) -> Result<Vec<Message>, anyhow::Error> {
    let data = fs::read(path)?;
    Ok(LogSegmentIter::new(data).collect())
}
