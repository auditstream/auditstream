//! Export batch carrying a slice of [`Event`]s and the next [`Cursor`].

use serde::{Deserialize, Serialize};

use crate::{Cursor, Event};

/// A batch of exported [`Event`] records together with the [`Cursor`] after its last event.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Batch {
    /// Events read in `(xact_id, id)` order within the export snapshot window.
    pub events: Vec<Event>,

    /// Cursor position after the last event in this batch (`None` when the batch is empty).
    pub cursor: Option<Cursor>,
}

impl Batch {
    /// Creates a new [`Batch`] from `events` and the resulting `cursor`.
    pub fn new(events: Vec<Event>, cursor: Option<Cursor>) -> Self {
        Self { events, cursor }
    }

    /// Returns `true` if the batch contains no events.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Returns the number of events in this batch.
    pub fn len(&self) -> usize {
        self.events.len()
    }
}
