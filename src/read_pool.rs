//! Fixed-size pool of read-only SQLite connections for the daemon.
//!
//! `tsmd` already runs one thread per client, so reads arrive concurrently.
//! WAL imposes no contention between reader connections, but a single
//! `Connection` cannot be shared across threads at once. The pool hands each
//! concurrent reader its own connection, so `N` connections give `N`-way real
//! parallel reads. Writes never use this pool (see ADR-0015).

use std::ops::Deref;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};

use rusqlite::Connection;

use crate::db;

struct Inner {
    conns: Mutex<Vec<Connection>>,
    available: Condvar,
}

pub struct ReadPool {
    inner: Arc<Inner>,
    size: usize,
}

impl ReadPool {
    pub fn new(db_path: &Path, size: usize) -> anyhow::Result<ReadPool> {
        let size = size.max(1);
        let mut conns = Vec::with_capacity(size);
        for _ in 0..size {
            conns.push(db::get_read_connection(db_path)?);
        }
        Ok(ReadPool {
            inner: Arc::new(Inner {
                conns: Mutex::new(conns),
                available: Condvar::new(),
            }),
            size,
        })
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// Check out a connection, blocking until one is free.
    pub fn checkout(&self) -> PooledConn {
        let mut guard = self.inner.conns.lock().expect("read pool poisoned");
        while guard.is_empty() {
            guard = self
                .inner
                .available
                .wait(guard)
                .expect("read pool poisoned");
        }
        let conn = guard.pop().expect("non-empty after wait");
        PooledConn {
            inner: Arc::clone(&self.inner),
            conn: Some(conn),
        }
    }
}

pub struct PooledConn {
    inner: Arc<Inner>,
    conn: Option<Connection>,
}

impl Deref for PooledConn {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn.as_ref().expect("connection present until drop")
    }
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            let mut guard = match self.inner.conns.lock() {
                Ok(g) => g,
                Err(e) => e.into_inner(),
            };
            guard.push(conn);
            self.inner.available.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        db::init_db(&path).unwrap();
        (dir, path)
    }

    #[test]
    fn test_new_opens_size_connections() {
        let (_dir, path) = temp_db();
        let pool = ReadPool::new(&path, 3).unwrap();
        assert_eq!(pool.size(), 3);
    }

    #[test]
    fn test_size_floor_is_one() {
        let (_dir, path) = temp_db();
        let pool = ReadPool::new(&path, 0).unwrap();
        assert_eq!(pool.size(), 1);
    }

    #[test]
    fn test_checkout_returns_usable_read_connection() {
        let (_dir, path) = temp_db();
        let pool = ReadPool::new(&path, 2).unwrap();
        let conn = pool.checkout();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn test_connection_returns_to_pool_on_drop() {
        let (_dir, path) = temp_db();
        let pool = ReadPool::new(&path, 1).unwrap();
        {
            let _c = pool.checkout(); // single connection checked out
        } // dropped, returned
          // If it was not returned, this second checkout would block forever.
        let _c2 = pool.checkout();
    }

    #[test]
    fn test_concurrent_checkouts_up_to_size() {
        let (_dir, path) = temp_db();
        let pool = Arc::new(ReadPool::new(&path, 4).unwrap());
        let mut handles = vec![];
        for _ in 0..4 {
            let p = Arc::clone(&pool);
            handles.push(std::thread::spawn(move || {
                let c = p.checkout();
                let _n: i64 = c
                    .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
                    .unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }
}
