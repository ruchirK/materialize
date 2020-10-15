// Copyright Materialize, Inc. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Types related to source persistence.
//
// TODO: currently everything is fairly Kafka-centric and we should probably
// not directly usable for some other source types.

use std::collections::HashMap;
use std::path::Path;
use std::pin::Pin;

use anyhow::Error;

use expr::GlobalId;
use futures::sink::Sink;
use log::error;
use repr::PersistedRecord;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::server::PersistenceMessage;

static RECORD_FILE_PREFIX: &str = "materialize";

/// Type alias for object that sends data to the persister.
pub type PersistenceSender = Pin<Box<dyn Sink<PersistenceMessage, Error = comm::Error> + Send>>;

/// Describes what is provided from a persisted file.
#[derive(Debug)]
pub struct RecordFileMetadata {
    /// The cluster id of the Materialize instance that wrote this file.
    pub cluster_id: Uuid,
    /// The source global ID this file represents.
    pub source_id: GlobalId,
    /// The partition ID this file represents.
    pub partition_id: i32,
    /// The inclusive lower bound of offsets provided by this file.
    pub start_offset: i64,
    /// The exclusive upper bound of offsets provided by this file.
    pub end_offset: i64,
    /// Whether or not this file is the first file for this partition.
    pub first: bool,
}

impl RecordFileMetadata {
    /// Parse a file's metadata from its path.
    pub fn from_path(path: &Path) -> Result<Option<Self>, Error> {
        let file_name = path.file_name();

        if file_name.is_none() {
            // Path ends in .. . This should never happen but let's
            // just ignore for now.
            return Ok(None);
        }

        let file_name = file_name.expect("known to have a file name").to_str();

        if file_name.is_none() {
            // Path cannot be converted to a UTF-8 string. This
            // should not be the case for persistence files as we
            // control every aspect of the name.
            // TODO(rkhaitan): Make sure this assumption is valid.
            return Ok(None);
        }

        let file_name = file_name.expect("known to be a valid UTF-8 file name");

        if !file_name.starts_with(RECORD_FILE_PREFIX) {
            // File name doesn't match the prefix we use to write
            // down persistence data.
            return Ok(None);
        }

        let parts: Vec<_> = file_name.split('-').collect();

        if parts.len() != 6 && parts.len() != 7 {
            // File is either partially written, or entirely irrelevant.
            error!(
                "Found invalid persistence file name: {}. Ignoring",
                file_name
            );
            return Ok(None);
        }

        if parts.len() == 7 && parts[6] != "first" {
            // We got a invalid filename.
            error!(
                "Found invalid persistence file name: {}. Ignoring",
                file_name
            );
            return Ok(None);
        }

        Ok(Some(Self {
            cluster_id: Uuid::parse_str(parts[1])?,
            source_id: parts[2].parse()?,
            partition_id: parts[3].parse()?,
            // Here we revert the transformation we made to convert this to a 0-indexed
            // offset in `generate_file_name`.
            start_offset: parts[4].parse::<i64>()? + 1,
            end_offset: parts[5].parse()?,
            first: parts.len() == 7,
        }))
    }

    /// Generate a file name that can later be parsed into metadata.
    pub fn generate_file_name(
        cluster_id: Uuid,
        source_id: GlobalId,
        partition_id: i32,
        start_offset: i64,
        end_offset: i64,
        first: bool,
    ) -> String {
        // We get start and end offsets as 1-indexed MzOffsets that denote the set of
        // offsets [start, end] (in 1-indexed offsets). Unfortunately, Kafka offsets are
        // actually 0-indexed, and therefore this construction is not easily explainable to
        // users. We will instead convert this to [start, end) in 0-indexed offsets.
        // TODO(rkhaitan): revisit MzOffsets being 1-indexed. This seems extremely confusing
        // for questionable value.
        assert!(
            start_offset > 0,
            "start offset has to be a valid 1-indexed offset"
        );
        let file_name = format!(
            "{}-{}-{}-{}-{}-{}",
            RECORD_FILE_PREFIX,
            cluster_id.to_simple(),
            source_id,
            partition_id,
            start_offset - 1,
            end_offset
        );

        if first {
            format!("{}-first", file_name)
        } else {
            file_name
        }
    }
}

/// Source data that gets sent to the persistence thread to place in persistent storage.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkerPersistenceData {
    /// Global Id of the Source whose data is being persisted.
    pub source_id: GlobalId,
    /// Partition the record belongs to.
    pub partition_id: i32,
    /// The record itself.
    pub record: PersistedRecord,
}

/// TODO blahblah
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PersistenceAddSource {
    /// Global Id of source
    pub source_id: GlobalId,
    /// Cluster id
    pub cluster_id: Uuid,
    /// Optional set of offsets that indicate records that have already been
    /// persisted.
    pub start_offsets: Option<HashMap<i32, i64>>,
}
