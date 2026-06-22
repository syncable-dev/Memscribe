//! The persisted byte-offset cursor store. The tailer keeps one offset per
//! transcript file so a restart resumes exactly where it left off — no
//! duplicates, no loss (whitepaper §7, §8.5).

use std::collections::HashMap;

/// A keyed store of byte offsets (key = a transcript file identity).
pub trait OffsetStore {
    /// The last persisted offset for `key`, if any.
    fn get(&self, key: &str) -> Option<u64>;
    /// Persist `offset` for `key`.
    fn set(&mut self, key: &str, offset: u64);
}

/// An in-memory offset store (tests, ephemeral runs).
#[derive(Debug, Default, Clone)]
pub struct MemoryOffsetStore {
    map: HashMap<String, u64>,
}

impl MemoryOffsetStore {
    /// A fresh, empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl OffsetStore for MemoryOffsetStore {
    fn get(&self, key: &str) -> Option<u64> {
        self.map.get(key).copied()
    }
    fn set(&mut self, key: &str, offset: u64) {
        self.map.insert(key.to_string(), offset);
    }
}

/// A SQLite-backed persistent offset store (feature `cursor-store`).
#[cfg(feature = "cursor-store")]
pub mod persistent {
    use super::OffsetStore;
    use rusqlite::Connection;
    use std::path::Path;

    /// A durable offset store backed by SQLite.
    pub struct SqliteOffsetStore {
        conn: Connection,
    }

    impl SqliteOffsetStore {
        /// Open (or create) the offset store at `path`.
        ///
        /// # Errors
        /// Returns a rusqlite error if the database cannot be opened.
        pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
            let conn = Connection::open(path)?;
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS offsets (key TEXT PRIMARY KEY, offset INTEGER NOT NULL);",
            )?;
            Ok(SqliteOffsetStore { conn })
        }
    }

    impl OffsetStore for SqliteOffsetStore {
        fn get(&self, key: &str) -> Option<u64> {
            self.conn
                .query_row("SELECT offset FROM offsets WHERE key = ?1", [key], |r| {
                    r.get::<_, i64>(0)
                })
                .ok()
                .map(|v| v as u64)
        }
        fn set(&mut self, key: &str, offset: u64) {
            let _ = self.conn.execute(
                "INSERT INTO offsets(key, offset) VALUES(?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET offset = ?2",
                rusqlite::params![key, offset as i64],
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_roundtrips() {
        let mut s = MemoryOffsetStore::new();
        assert_eq!(s.get("a"), None);
        s.set("a", 42);
        assert_eq!(s.get("a"), Some(42));
        s.set("a", 100);
        assert_eq!(s.get("a"), Some(100));
    }
}
