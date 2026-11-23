//! Remote directories.

use std::collections::VecDeque;

use super::session::PendingReply;
use super::{Handle, Session};
use crate::error::{Error, ProtocolError};
use crate::wire::{NameEntry, Op, Reply};

use super::ops::{is_eof, unexpected};

/// Sends a close request for a handle that is being dropped, if a runtime is
/// available. Best effort: nothing waits for it and its failure is ignored.
pub(super) fn close_in_background(session: &Session, handle: &Handle) {
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        let (session, handle) = (session.clone(), handle.clone());
        runtime.spawn(async move {
            let _ = session.close_handle(&handle).await;
        });
    }
}

/// An open remote directory, read entry by entry. Opened with
/// [`Dir::open`] or [`fs::read_dir`](super::fs::read_dir).
///
/// The entries `.` and `..` are skipped. Entries come in the order the
/// server lists them. Dropping a directory sends a close request in the
/// background, best effort; call [`close`](Self::close) to be sure.
pub struct Dir {
    session: Session,
    handle: Handle,
    entries: VecDeque<NameEntry>,
    inflight: Option<PendingReply>,
    done: bool,
    closed: bool,
}

impl Dir {
    /// Opens the directory at `path`.
    pub async fn open(session: &Session, path: impl AsRef<[u8]>) -> Result<Self, Error> {
        let handle = session.opendir(path).await?;
        Ok(Self {
            session: session.clone(),
            handle,
            entries: VecDeque::new(),
            inflight: None,
            done: false,
            closed: false,
        })
    }

    /// The next entry, or `None` at the end of the listing.
    ///
    /// Cancel safe: a batch requested by a call that was dropped is picked up
    /// by the next call.
    pub async fn next_entry(&mut self) -> Result<Option<NameEntry>, Error> {
        loop {
            while let Some(entry) = self.entries.pop_front() {
                if entry.name != "." && entry.name != ".." {
                    return Ok(Some(entry));
                }
            }
            if self.done {
                return Ok(None);
            }
            if self.inflight.is_none() {
                let slot = self.session.reserve().await?;
                self.inflight = Some(slot.send(Op::ReadDir {
                    handle: self.handle.0.clone(),
                })?);
            }
            let Some(pending) = self.inflight.as_mut() else {
                continue;
            };
            let reply = std::future::poll_fn(|cx| pending.poll_reply(cx)).await;
            self.inflight = None;
            match reply? {
                Reply::Name(batch) if batch.is_empty() => {
                    return Err(Error::Protocol(ProtocolError::NoProgress));
                }
                Reply::Name(batch) => self.entries.extend(batch),
                other if is_eof(&other) => self.done = true,
                other => return Err(unexpected(other, "name")),
            }
        }
    }

    /// Closes the directory on the server.
    pub async fn close(mut self) -> Result<(), Error> {
        self.closed = true;
        self.session.close_handle(&self.handle).await
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        if !self.closed {
            close_in_background(&self.session, &self.handle);
        }
    }
}
