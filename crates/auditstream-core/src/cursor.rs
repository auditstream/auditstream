//! Export cursor ordered by `(xact_id, id)` per `spec/history-table.md`.

use serde::{Deserialize, Serialize};

/// Position in the `auditstream.changes` export stream.
///
/// Ordered lexicographically by `(xact_id, id)` because `id` alone reflects insert order
/// rather than transaction commit order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Cursor {
    /// 64-bit PostgreSQL transaction ID (`xid8`).
    pub xact_id: u64,

    /// Monotonic row identifier within `auditstream.changes` (`bigint`).
    pub id: i64,
}

impl Cursor {
    /// Creates a new [`Cursor`] at `(xact_id, id)`.
    pub const fn new(xact_id: u64, id: i64) -> Self {
        Self { xact_id, id }
    }
}
