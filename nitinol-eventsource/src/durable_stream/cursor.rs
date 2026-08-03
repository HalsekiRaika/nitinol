use nitinol_persistence::{LoadQuery, LoadedEvent};

/// Pointer to the next event a [`DurableStream`](super::DurableStream) should
/// deliver.
///
/// `after` follows checkpoint semantics: it stores the last sequence number
/// that has been successfully observed.  The next poll fetches events whose
/// relevant sequence is strictly greater than `after`.
#[derive(Debug, Clone)]
pub enum SequenceCursor {
    /// Poll a single aggregate stream identified by `key`, ordered by the
    /// per-stream `sequence` number.
    Stream { key: String, after: u64 },
    /// Poll across every stream in the event store, ordered by the
    /// `global_sequence` number.
    Global { after: u64 },
}

impl SequenceCursor {
    /// Build the [`LoadQuery`] that fetches every event past the cursor's
    /// current position.
    pub(crate) fn to_load_query(&self) -> LoadQuery {
        match self {
            SequenceCursor::Stream { key, after } => LoadQuery {
                stream_key: Some(key.clone()),
                from_stream_sequence: Some(after + 1),
                ..Default::default()
            },
            SequenceCursor::Global { after } => LoadQuery {
                from_global_sequence: Some(after + 1),
                ..Default::default()
            },
        }
    }

    /// Sequence number of `loaded` in the dimension the cursor tracks.
    pub(crate) fn observed(&self, loaded: &LoadedEvent) -> u64 {
        match self {
            SequenceCursor::Stream { .. } => loaded.sequence,
            SequenceCursor::Global { .. } => loaded.global_sequence,
        }
    }

    /// Advance the cursor's `after` value to `sequence`.
    pub(crate) fn advance(&mut self, sequence: u64) {
        match self {
            SequenceCursor::Stream { after, .. } => *after = sequence,
            SequenceCursor::Global { after } => *after = sequence,
        }
    }
}
