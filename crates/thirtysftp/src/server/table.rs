//! What the tasks of one connection share: the file system, the limits and
//! the table of open handles, with the order of requests on each handle.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use tokio::sync::oneshot;

use super::config::ServerConfig;
use super::filesystem::{Filesystem, FsError};
use crate::wire::NameEntry;

/// Everything the tasks of one connection share.
pub(super) struct Ctx<F: Filesystem> {
    pub(super) fs: Arc<F>,
    pub(super) cfg: ServerConfig,
    pub(super) table: Mutex<Table<F>>,
    /// Extension names the server announced and will dispatch.
    pub(super) announced: Vec<&'static str>,
}

pub(super) struct Table<F: Filesystem> {
    pub(super) entries: HashMap<u64, Entry<F>>,
    /// Opens in progress, counted against the handle limit.
    pub(super) opening: usize,
    pub(super) next: u64,
}

pub(super) struct Entry<F: Filesystem> {
    pub(super) kind: Kind<F>,
    /// Completes when the last request queued on the handle has finished.
    pub(super) tail: Option<oneshot::Receiver<()>>,
}

pub(super) enum Kind<F: Filesystem> {
    File(Arc<F::File>),
    Dir(Arc<DirState<F>>),
}

impl<F: Filesystem> Clone for Kind<F> {
    fn clone(&self) -> Self {
        match self {
            Self::File(f) => Self::File(f.clone()),
            Self::Dir(d) => Self::Dir(d.clone()),
        }
    }
}

pub(super) struct DirState<F: Filesystem> {
    pub(super) dir: F::Dir,
    /// Entries the file system returned that the client has not seen yet.
    pub(super) pending: Mutex<VecDeque<NameEntry>>,
    pub(super) finished: AtomicBool,
}

/// A place in the request order of one handle.
pub(super) struct Order {
    pub(super) prev: Option<oneshot::Receiver<()>>,
    pub(super) _done: oneshot::Sender<()>,
}

impl Order {
    pub(super) async fn wait(&mut self) {
        if let Some(prev) = self.prev.take() {
            // The previous request ending drops its sender, which is the
            // signal; an error here means exactly that.
            let _ = prev.await;
        }
    }
}

pub(super) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(super) fn handle_key(handle: &[u8]) -> Option<u64> {
    Some(u64::from_be_bytes(handle.try_into().ok()?))
}

impl<F: Filesystem> Ctx<F> {
    /// Finds a handle and takes the next place in its request order.
    pub(super) fn lookup(&self, handle: &[u8]) -> Option<(Kind<F>, Order)> {
        let key = handle_key(handle)?;
        let mut table = lock(&self.table);
        let entry = table.entries.get_mut(&key)?;
        let (done, signal) = oneshot::channel();
        let order = Order {
            prev: entry.tail.replace(signal),
            _done: done,
        };
        Some((entry.kind.clone(), order))
    }

    /// Counts an open against the handle limit.
    pub(super) fn reserve(&self) -> Result<Reservation<'_, F>, FsError> {
        let mut table = lock(&self.table);
        if table.entries.len() + table.opening >= self.cfg.max_handles {
            return Err(FsError::failure("too many open handles"));
        }
        table.opening += 1;
        Ok(Reservation { ctx: self })
    }

    pub(super) fn insert(&self, _reservation: &Reservation<'_, F>, kind: Kind<F>) -> Bytes {
        let mut table = lock(&self.table);
        table.next += 1;
        let key = table.next;
        table.entries.insert(key, Entry { kind, tail: None });
        Bytes::copy_from_slice(&key.to_be_bytes())
    }
}

pub(super) struct Reservation<'a, F: Filesystem> {
    pub(super) ctx: &'a Ctx<F>,
}

impl<F: Filesystem> Drop for Reservation<'_, F> {
    fn drop(&mut self) {
        lock(&self.ctx.table).opening -= 1;
    }
}
