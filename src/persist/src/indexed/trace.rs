// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! A persistent, compacting data structure of `(Key, Value, Time, Diff)`
//! updates, indexed by key.
//!
//! This is directly a persistent analog of [differential_dataflow::trace::Trace].

use std::marker::PhantomData;
use std::sync::Arc;

use differential_dataflow::trace::Description;
use timely::progress::{Antichain, Timestamp};
use timely::PartialOrder;

use crate::error::Error;
use crate::indexed::cache::BlobCache;
use crate::indexed::encoding::BlobTraceMeta;
use crate::indexed::{BlobTraceBatch, Id, Snapshot};
use crate::storage::Blob;
use crate::Data;

/// A persistent, compacting data structure containing `(Key, Value, Time,
/// Diff)` entries indexed by `(key, value, time)`.
///
/// Invariants:
/// - All entries are before some time frontier.
/// - This acts as an append-only log. Data is added in advancing batches and
///   logically immutable after that (modulo compactions, which preserve it, but
///   the ability to read at old times is lost).
/// - TODO: Explain since and logical compactions.
/// - TODO: Space usage.
pub struct BlobTrace<K, V> {
    id: Id,
    // The next ID used to assign a Blob key for this trace.
    next_blob_id: u64,
    since: Antichain<u64>,
    // NB: The Descriptions here are sorted and contiguous half-open intervals
    // `[lower, upper)`.
    batches: Vec<(Description<u64>, String)>,
    _phantom: PhantomData<(K, V)>,
}

impl<K: Data, V: Data> BlobTrace<K, V> {
    /// Returns a BlobTrace re-instantiated with the previously serialized
    /// state.
    pub fn new(meta: BlobTraceMeta) -> Self {
        BlobTrace {
            id: meta.id,
            next_blob_id: meta.next_blob_id,
            since: meta.since,
            batches: meta.batches.clone(),
            _phantom: PhantomData,
        }
    }

    fn new_blob_key(&mut self) -> String {
        let key = format!("{:?}-trace-{:?}", self.id, self.next_blob_id);
        self.next_blob_id += 1;

        key
    }

    /// Serializes the state of this BlobTrace for later re-instantiation.
    pub fn meta(&self) -> BlobTraceMeta {
        BlobTraceMeta {
            id: self.id,
            batches: self.batches.clone(),
            next_blob_id: self.next_blob_id,
            since: self.since.clone(),
        }
    }

    /// An upper bound on the times of contained updates.
    pub fn ts_upper(&self) -> Antichain<u64> {
        match self.batches.last() {
            Some((desc, _)) => desc.upper().clone(),
            None => Antichain::from_elem(Timestamp::minimum()),
        }
    }

    /// A lower bound on the time at which updates may have been logically
    /// compacted together.
    pub fn since(&self) -> Antichain<u64> {
        self.since.clone()
    }

    /// Writes the given batch to [Blob] storage and logically adds the contained
    /// updates to this trace.
    pub fn append<L: Blob>(
        &mut self,
        batch: BlobTraceBatch<K, V>,
        blob: &mut BlobCache<K, V, L>,
    ) -> Result<(), Error> {
        if &self.ts_upper() != batch.desc.lower() {
            return Err(Error::from(format!(
                "batch lower doesn't match trace upper {:?}: {:?}",
                self.ts_upper(),
                batch.desc
            )));
        }
        let desc = batch.desc.clone();
        let key = self.new_blob_key();
        blob.set_trace_batch(key.clone(), batch)?;
        self.batches.push((desc, key));
        Ok(())
    }

    /// Compact the subrange of batches containing times < as_of to a single batch from
    /// [0, max(batch_upper <= as_of)) and forward all updates in that batch to max(batch_upper <= as_of) - 1
    ///
    /// In general we would want to compact all updates <= as_of into a single batch and split the batch containing updates
    /// [lo, hi) s.t. lo <= as_of < hi into two batches [lo, as_of + 1), [as_of + 1, hi) and then do the compaction aligned to
    /// a batch boundary but this was a simpler place to start.
    pub fn compact<L: Blob>(
        &mut self,
        as_of: Antichain<u64>,
        blob: &mut BlobCache<K, V, L>,
    ) -> Result<(), Error> {
        if PartialOrder::less_equal(&self.ts_upper(), &as_of) {
            return Err(Error::from(format!("invalid compaction")));
        }

        // TODO
        // check the lower and since as well

        // Need to pull out all of the batches with upper <= as_of
        let mut to_compact = vec![];
        for (desc, key) in self.batches.iter() {
            if PartialOrder::less_equal(desc.upper(), &as_of) {
                to_compact.push((desc, key));
            }
        }

        if to_compact.is_empty() {
            return Ok(());
        }

        // Now we need to extract all of the records from all of those batches.
        let mut updates = vec![];

        for (_, key) in to_compact.iter() {
            updates.push(blob.get_trace_batch(key)?);
        }

        let mut buf = vec![];

        for batch in updates.iter() {
            buf.extend(batch.updates.iter().cloned());
        }

        // Get lower and upper bounds from the batches we just looked at
        let lower = updates.first().expect("known to exist").desc.lower();
        let upper = updates.last().expect("known to exist").desc.upper();
        // We know since we don't allow empty batches that lower has to be
        // strictly less than upper.
        debug_assert!(PartialOrder::less_than(lower, upper));
        let as_of_time = upper[0].saturating_sub(1);

        // Forward the times of those updates to upper
        for ((_, _), t, _) in buf.iter_mut() {
            *t = as_of_time;
        }

        // Consolidate
        differential_dataflow::consolidation::consolidate_updates(&mut buf);

        // Construct a new BlobTraceBatch
        let desc = Description::new(
            lower.clone(),
            upper.clone(),
            Antichain::from_elem(as_of_time),
        );
        let new_batch = BlobTraceBatch {
            desc: desc.clone(),
            updates: buf,
        };

        let key = self.new_blob_key();
        blob.set_trace_batch(key.clone(), new_batch)?;
        self.batches
            .retain(|(desc, _)| !PartialOrder::less_equal(desc.upper(), &as_of));
        self.batches.insert(0, (desc, key));
        self.since = as_of;

        // TODO actually clear the unwanted batches from the blob storage

        Ok(())
    }

    /// Returns a consistent read of all the updates contained in this trace.
    pub fn snapshot<L: Blob>(
        &self,
        blob: &BlobCache<K, V, L>,
    ) -> Result<TraceSnapshot<K, V>, Error> {
        let ts_upper = self.ts_upper();
        let mut updates = Vec::with_capacity(self.batches.len());
        for (_, key) in self.batches.iter() {
            updates.push(blob.get_trace_batch(key)?);
        }
        Ok(TraceSnapshot { ts_upper, updates })
    }
}

/// A consistent snapshot of the data currently in a persistent [BlobTrace].
#[derive(Debug)]
pub struct TraceSnapshot<K, V> {
    /// An open upper bound on the times of contained updates.
    pub ts_upper: Antichain<u64>,
    updates: Vec<Arc<BlobTraceBatch<K, V>>>,
}

impl<K: Clone, V: Clone> Snapshot<K, V> for TraceSnapshot<K, V> {
    fn read<E: Extend<((K, V), u64, isize)>>(&mut self, buf: &mut E) -> bool {
        if let Some(batch) = self.updates.pop() {
            buf.extend(batch.updates.iter().cloned());
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use crate::indexed::encoding::Id;
    use crate::indexed::SnapshotExt;
    use crate::mem::MemBlob;

    use super::*;

    #[test]
    fn trace_compact() -> Result<(), Error> {
        let mut blob = BlobCache::new(MemBlob::new("trace_compact")?);
        let mut t = BlobTrace::new(BlobTraceMeta::new(Id(0)));

        let batch = BlobTraceBatch {
            desc: Description::new(
                Antichain::from_elem(0),
                Antichain::from_elem(1),
                Antichain::from_elem(0),
            ),
            updates: vec![(("k".to_string(), "v".to_string()), 0, 1)],
        };

        assert_eq!(t.append(batch, &mut blob), Ok(()));
        let batch = BlobTraceBatch {
            desc: Description::new(
                Antichain::from_elem(1),
                Antichain::from_elem(3),
                Antichain::from_elem(0),
            ),
            updates: vec![(("k".to_string(), "v".to_string()), 2, 1)],
        };
        assert_eq!(t.append(batch, &mut blob), Ok(()));

        let batch = BlobTraceBatch {
            desc: Description::new(
                Antichain::from_elem(3),
                Antichain::from_elem(9),
                Antichain::from_elem(0),
            ),
            updates: vec![(("k".to_string(), "v".to_string()), 5, 1)],
        };
        assert_eq!(t.append(batch, &mut blob), Ok(()));

        t.compact(Antichain::from_elem(3), &mut blob)?;

        let snapshot = t.snapshot(&blob)?;

        let updates = snapshot.read_to_end();

        assert_eq!(
            updates,
            vec![
                (("k".to_string(), "v".to_string()), 2, 2),
                (("k".to_string(), "v".to_string()), 5, 1)
            ]
        );

        Ok(())
    }
}
