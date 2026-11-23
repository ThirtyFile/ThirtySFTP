//! Remote files.

use std::collections::VecDeque;
use std::future::Future;
use std::io::{self, SeekFrom};
use std::pin::Pin;
use std::sync::{Mutex, PoisonError};
use std::task::{Context, Poll, ready};

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};

use super::dir::close_in_background;
use super::ops::{bytes, check_ok, check_read};
use super::session::{PendingReply, Slot};
use super::{Handle, Session};
use crate::error::{Error, ProtocolError};
use crate::wire::{Attributes, Op, OpenFlags};

/// A boxed future that can sit in a `Sync` struct. The future is only ever
/// polled through `&mut`, so the mutex is never contended; it is there to
/// make the type `Sync`, which an `&File` held across an await needs.
type Boxed<T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send>>;

struct BoxFuture<T>(Mutex<Boxed<T>>);

impl<T> BoxFuture<T> {
    fn new(future: impl Future<Output = Result<T, Error>> + Send + 'static) -> Self {
        Self(Mutex::new(Box::pin(future)))
    }

    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Result<T, Error>> {
        self.0
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
            .poll(cx)
    }
}

fn invalid(what: &'static str) -> Error {
    io::Error::new(io::ErrorKind::InvalidInput, what).into()
}

/// Options for opening a remote file, in the manner of
/// [`std::fs::OpenOptions`].
#[derive(Clone, Debug, Default)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    create: bool,
    truncate: bool,
    create_new: bool,
    mode: Option<u32>,
}

impl OpenOptions {
    /// Options with everything off.
    pub fn new() -> Self {
        Self::default()
    }

    /// Open for reading.
    pub fn read(&mut self, on: bool) -> &mut Self {
        self.read = on;
        self
    }

    /// Open for writing.
    pub fn write(&mut self, on: bool) -> &mut Self {
        self.write = on;
        self
    }

    /// Append to the end on every write. Implies write access.
    pub fn append(&mut self, on: bool) -> &mut Self {
        self.append = on;
        self
    }

    /// Create the file if it does not exist. Needs write or append.
    pub fn create(&mut self, on: bool) -> &mut Self {
        self.create = on;
        self
    }

    /// Cut an existing file to length 0. Needs write.
    pub fn truncate(&mut self, on: bool) -> &mut Self {
        self.truncate = on;
        self
    }

    /// Create the file, failing if it exists. Needs write or append.
    pub fn create_new(&mut self, on: bool) -> &mut Self {
        self.create_new = on;
        self
    }

    /// Permission bits for a file this call creates.
    pub fn mode(&mut self, mode: u32) -> &mut Self {
        self.mode = Some(mode);
        self
    }

    /// Opens the file.
    pub async fn open(&self, session: &Session, path: impl AsRef<[u8]>) -> Result<File, Error> {
        let writing = self.write || self.append;
        if !self.read && !writing {
            return Err(invalid("open needs read, write or append"));
        }
        if !writing && (self.create || self.create_new) {
            return Err(invalid("create needs write or append"));
        }
        if self.truncate && !self.write {
            return Err(invalid("truncate needs write"));
        }
        let mut flags = 0;
        if self.read {
            flags |= OpenFlags::READ.0;
        }
        if writing {
            flags |= OpenFlags::WRITE.0;
        }
        if self.append {
            flags |= OpenFlags::APPEND.0;
        }
        if self.create || self.create_new {
            flags |= OpenFlags::CREATE.0;
        }
        if self.truncate {
            flags |= OpenFlags::TRUNCATE.0;
        }
        if self.create_new {
            flags |= OpenFlags::EXCLUSIVE.0;
        }
        let attrs = Attributes {
            permissions: self.mode,
            ..Attributes::default()
        };
        let handle = session.open(path, OpenFlags(flags), attrs).await?;
        Ok(File::new(session.clone(), handle))
    }
}

/// State of a pending seek.
enum Seek {
    Idle,
    Done(io::Result<u64>),
    /// Waiting for the file size, then moving by the offset from the end.
    FromEnd(BoxFuture<Attributes>, i64),
}

/// An open remote file.
///
/// Positioned methods ([`read_at`](Self::read_at),
/// [`write_all_at`](Self::write_all_at), ...) take an offset and leave the
/// file's cursor alone. The `AsyncRead`, `AsyncWrite` and `AsyncSeek`
/// implementations use the cursor. Writes through `AsyncWrite` are pipelined
/// (several requests are in flight before the first reply is looked at) and
/// report a failure from an earlier write on a later call; call
/// `flush` or [`close`](Self::close) to learn the final outcome. Reads through
/// `AsyncRead` are read ahead: the file keeps several requests in flight, more
/// as the reader keeps going, so `tokio::io::copy` between a remote file and a
/// local one moves data at full speed without holding the file in memory.
/// [`read_range`](Self::read_range), [`read_all`](Self::read_all) and
/// [`write_all_at`](Self::write_all_at) do the same for a range held in
/// memory.
///
/// Dropping a file sends a close request in the background if a Tokio
/// runtime is running. That is best effort and not guaranteed to be sent or
/// to succeed; call [`close`](Self::close) to be sure.
pub struct File {
    session: Session,
    handle: Handle,
    closed: bool,
    /// The cursor: the next byte `AsyncRead` returns or `AsyncWrite` writes.
    pos: u64,

    /// Data already fetched but not yet handed to the reader.
    leftover: Bytes,
    reserving_read: Option<BoxFuture<Slot>>,
    /// Reads sent ahead of the reader, in file order: offset, length asked
    /// for, reply. The first one starts where `leftover` ends.
    read_ahead: VecDeque<(u64, u32, PendingReply)>,
    /// How many reads to keep in flight. Starts at one and doubles with each
    /// full reply, so a short file costs few wasted requests and a long one
    /// soon uses the whole pipeline.
    read_window: usize,

    reserving_write: Option<BoxFuture<Slot>>,
    writes: VecDeque<PendingReply>,
    write_error: Option<Error>,

    seek: Seek,
    closing: Option<BoxFuture<()>>,
}

impl File {
    fn new(session: Session, handle: Handle) -> Self {
        Self {
            session,
            handle,
            closed: false,
            pos: 0,
            leftover: Bytes::new(),
            reserving_read: None,
            read_ahead: VecDeque::new(),
            read_window: 1,
            reserving_write: None,
            writes: VecDeque::new(),
            write_error: None,
            seek: Seek::Idle,
            closing: None,
        }
    }

    /// Opens a file for reading.
    pub async fn open(session: &Session, path: impl AsRef<[u8]>) -> Result<Self, Error> {
        OpenOptions::new().read(true).open(session, path).await
    }

    /// Creates a file for writing, cutting it to length 0 if it exists.
    pub async fn create(session: &Session, path: impl AsRef<[u8]>) -> Result<Self, Error> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(session, path)
            .await
    }

    /// The server's handle for this file.
    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    /// The session the file belongs to.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Attributes of the file.
    pub async fn metadata(&self) -> Result<Attributes, Error> {
        self.session.fstat(&self.handle).await
    }

    /// Changes attributes of the file.
    pub async fn set_attributes(&self, attrs: Attributes) -> Result<(), Error> {
        self.session.fsetstat(&self.handle, attrs).await
    }

    /// Cuts or extends the file to `size` bytes.
    pub async fn set_len(&self, size: u64) -> Result<(), Error> {
        self.set_attributes(Attributes {
            size: Some(size),
            ..Attributes::default()
        })
        .await
    }

    /// Flushes the file to stable storage on the server. Needs the
    /// `fsync@openssh.com` extension.
    pub async fn sync_all(&self) -> Result<(), Error> {
        self.session.fsync(&self.handle).await
    }

    /// Reads up to `buf.len()` bytes at `offset` in one request, and returns
    /// how many were read. 0 means the end of the file (or an empty `buf`).
    /// The server may return fewer bytes than asked for.
    pub async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        let len = u32::try_from(buf.len())
            .unwrap_or(u32::MAX)
            .min(self.session.read_chunk());
        if len == 0 {
            return Ok(0);
        }
        match self.session.read(&self.handle, offset, len).await? {
            None => Ok(0),
            Some(data) if data.is_empty() => Err(Error::Protocol(ProtocolError::NoProgress)),
            Some(data) => {
                buf[..data.len()].copy_from_slice(&data);
                Ok(data.len())
            }
        }
    }

    /// Writes up to `data.len()` bytes at `offset` in one request, and
    /// returns how many were written.
    pub async fn write_at(&self, offset: u64, data: &[u8]) -> Result<usize, Error> {
        let n = data
            .len()
            .min(self.session.write_chunk(self.handle.0.len()));
        if n == 0 {
            return Ok(0);
        }
        self.session
            .write(&self.handle, offset, Bytes::copy_from_slice(&data[..n]))
            .await?;
        Ok(n)
    }

    /// Reads `len` bytes starting at `offset`, or fewer if the file ends
    /// first.
    ///
    /// The range is split into requests that are kept in flight together. If
    /// the server answers a request with less data than asked for, the missing
    /// part is requested again before anything later is used, so the result is
    /// always the contiguous bytes of the file. Memory use grows with the data
    /// actually received, not with `len`.
    pub async fn read_range(&self, offset: u64, len: u64) -> Result<Vec<u8>, Error> {
        read_range(&self.session, &self.handle, offset, len).await
    }

    /// Reads the file from the start to its end. A size reported by the
    /// server is used to read ahead in parallel; the read then continues
    /// until the server reports the end, so a file that grew meanwhile, or
    /// one that reports no size, is still read completely.
    pub async fn read_all(&self) -> Result<Vec<u8>, Error> {
        let size = self.metadata().await?.size.unwrap_or(0);
        let mut out = self.read_range(0, size).await?;
        // Whatever came back short of `size` means the file ended; anything
        // more is only possible if it did not.
        if out.len() as u64 == size {
            loop {
                let more = self
                    .read_range(out.len() as u64, u64::from(self.session.read_chunk()) * 16)
                    .await?;
                if more.is_empty() {
                    break;
                }
                out.extend_from_slice(&more);
            }
        }
        Ok(out)
    }

    /// Writes all of `data` starting at `offset`, with many requests in
    /// flight. If a request fails, the error is returned and the range may be
    /// partly written.
    pub async fn write_all_at(&self, offset: u64, data: impl Into<Bytes>) -> Result<(), Error> {
        write_all(&self.session, &self.handle, offset, data.into()).await
    }

    /// Waits for pending writes, then closes the file on the server.
    pub async fn close(mut self) -> Result<(), Error> {
        let flushed = self.drain_writes().await;
        self.closed = true;
        let closed = self.session.close_handle(&self.handle).await;
        flushed.and(closed)
    }

    async fn drain_writes(&mut self) -> Result<(), Error> {
        let mut result = match self.write_error.take() {
            Some(e) => Err(e),
            None => Ok(()),
        };
        while let Some(p) = self.writes.pop_front() {
            let r = p.wait().await.and_then(check_ok);
            if result.is_ok() {
                result = r;
            }
        }
        result
    }

    /// Forgets read-ahead state; the cursor stays where the reader is.
    fn reset_read(&mut self) {
        self.leftover = Bytes::new();
        self.read_ahead.clear();
        self.read_window = 1;
        self.reserving_read = None;
    }

    /// Collects replies of writes that are already there; keeps the first
    /// error.
    fn reap_writes(&mut self, cx: &mut Context<'_>) {
        while let Some(front) = self.writes.front_mut() {
            match front.poll_reply(cx) {
                Poll::Pending => break,
                Poll::Ready(r) => {
                    self.writes.pop_front();
                    if let Err(e) = r.and_then(check_ok) {
                        self.write_error.get_or_insert(e);
                    }
                }
            }
        }
    }
}

impl Drop for File {
    fn drop(&mut self) {
        if !self.closed {
            close_in_background(&self.session, &self.handle);
        }
    }
}

impl std::fmt::Debug for File {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("File")
            .field("handle", &self.handle)
            .field("pos", &self.pos)
            .finish_non_exhaustive()
    }
}

impl AsyncRead for File {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if !this.leftover.is_empty() {
            let n = this.leftover.len().min(buf.remaining());
            buf.put_slice(&this.leftover.split_to(n));
            this.pos += n as u64;
            return Poll::Ready(Ok(()));
        }
        // Keep `read_window` reads in flight, each following the last.
        let chunk = this.session.read_chunk();
        let mut next = match this.read_ahead.back() {
            Some((at, len, _)) => at.checked_add(u64::from(*len)),
            None => Some(this.pos),
        };
        while this.read_ahead.len() < this.read_window {
            let Some(at) = next else { break };
            let session = this.session.clone();
            let fut = this
                .reserving_read
                .get_or_insert_with(|| BoxFuture::new(async move { session.reserve().await }));
            let Poll::Ready(slot) = fut.poll(cx) else {
                break;
            };
            this.reserving_read = None;
            let pending = slot?.send(Op::Read {
                handle: this.handle.0.clone(),
                offset: at,
                len: chunk,
            })?;
            this.read_ahead.push_back((at, chunk, pending));
            next = at.checked_add(u64::from(chunk));
        }
        let Some((_, want, pending)) = this.read_ahead.front_mut() else {
            // Waiting for a request slot; the reservation holds the waker.
            return Poll::Pending;
        };
        let want = *want;
        let reply = ready!(pending.poll_reply(cx));
        this.read_ahead.pop_front();
        match check_read(reply?, want)? {
            None => {
                // The end of the file: what was read ahead is beyond it. A
                // later read starts afresh, in case the file has grown.
                this.read_ahead.clear();
                this.read_window = 1;
                Poll::Ready(Ok(()))
            }
            Some(data) if data.is_empty() => {
                Poll::Ready(Err(Error::Protocol(ProtocolError::NoProgress).into()))
            }
            Some(mut data) => {
                if (data.len() as u32) < want {
                    // A short reply leaves a gap before the reads sent after
                    // it, so they are dropped and sent again from the gap.
                    this.read_ahead.clear();
                } else {
                    this.read_window = (this.read_window * 2).min(this.session.pipeline_depth());
                }
                let n = data.len().min(buf.remaining());
                buf.put_slice(&data.split_to(n));
                this.pos += n as u64;
                this.leftover = data;
                Poll::Ready(Ok(()))
            }
        }
    }
}

impl AsyncWrite for File {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if data.is_empty() {
            return Poll::Ready(Ok(0));
        }
        this.reap_writes(cx);
        if let Some(e) = &this.write_error {
            return Poll::Ready(Err(e.clone().into()));
        }
        // Wait for the oldest request when the window is full.
        while this.writes.len() >= this.session.pipeline_depth() {
            let Some(front) = this.writes.front_mut() else {
                break;
            };
            let r = ready!(front.poll_reply(cx));
            this.writes.pop_front();
            if let Err(e) = r.and_then(check_ok) {
                return Poll::Ready(Err(this.write_error.insert(e).clone().into()));
            }
        }
        let session = this.session.clone();
        let fut = this
            .reserving_write
            .get_or_insert_with(|| BoxFuture::new(async move { session.reserve().await }));
        let slot = ready!(fut.poll(cx));
        this.reserving_write = None;

        // A write moves the cursor, so anything read ahead is stale.
        this.reset_read();
        let n = data
            .len()
            .min(this.session.write_chunk(this.handle.0.len()));
        let pending = slot?.send(Op::Write {
            handle: this.handle.0.clone(),
            offset: this.pos,
            data: bytes(&data[..n]),
        })?;
        this.writes.push_back(pending);
        this.pos += n as u64;
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        while let Some(front) = this.writes.front_mut() {
            let r = ready!(front.poll_reply(cx));
            this.writes.pop_front();
            if let Err(e) = r.and_then(check_ok) {
                this.write_error.get_or_insert(e);
            }
        }
        match &this.write_error {
            Some(e) => Poll::Ready(Err(e.clone().into())),
            None => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let flushed = ready!(self.as_mut().poll_flush(cx));
        let this = self.get_mut();
        if this.closed {
            return Poll::Ready(flushed);
        }
        let session = this.session.clone();
        let handle = this.handle.clone();
        let fut = this.closing.get_or_insert_with(|| {
            BoxFuture::new(async move { session.close_handle(&handle).await })
        });
        let closed = ready!(fut.poll(cx));
        this.closing = None;
        this.closed = true;
        Poll::Ready(flushed.and(closed.map_err(Into::into)))
    }
}

impl AsyncSeek for File {
    fn start_seek(self: Pin<&mut Self>, position: SeekFrom) -> io::Result<()> {
        let this = self.get_mut();
        if !matches!(this.seek, Seek::Idle) {
            return Err(io::Error::other("a seek is already in progress"));
        }
        this.reset_read();
        let before_start = || io::Error::new(io::ErrorKind::InvalidInput, "seek before the start");
        this.seek = match position {
            SeekFrom::Start(n) => Seek::Done(Ok(n)),
            SeekFrom::Current(d) => {
                Seek::Done(this.pos.checked_add_signed(d).ok_or_else(before_start))
            }
            SeekFrom::End(d) => {
                let session = this.session.clone();
                let handle = this.handle.clone();
                Seek::FromEnd(
                    BoxFuture::new(async move { session.fstat(&handle).await }),
                    d,
                )
            }
        };
        Ok(())
    }

    fn poll_complete(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        let this = self.get_mut();
        if let Seek::FromEnd(fut, delta) = &mut this.seek {
            let delta = *delta;
            let attrs = ready!(fut.poll(cx));
            this.seek = Seek::Done(match attrs {
                Err(e) => Err(e.into()),
                Ok(a) => a
                    .size
                    .and_then(|size| size.checked_add_signed(delta))
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "seek before the start")
                    }),
            });
        }
        match std::mem::replace(&mut this.seek, Seek::Idle) {
            Seek::Idle => Poll::Ready(Ok(this.pos)),
            Seek::Done(r) => {
                if let Ok(pos) = &r {
                    this.pos = *pos;
                }
                Poll::Ready(r)
            }
            Seek::FromEnd(..) => Poll::Pending,
        }
    }
}

/// Reads `len` bytes at `offset` with many requests in flight. See
/// [`File::read_range`].
pub(super) async fn read_range(
    session: &Session,
    handle: &Handle,
    offset: u64,
    len: u64,
) -> Result<Vec<u8>, Error> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| invalid("range passes the largest offset"))?;
    let chunk = u64::from(session.read_chunk());
    let depth = session.pipeline_depth();
    let mut out = Vec::new();
    let mut next = offset;
    // Requests in file order: (offset, length asked for, reply).
    let mut queue: VecDeque<(u64, u32, PendingReply)> = VecDeque::new();
    loop {
        while queue.len() < depth && next < end {
            let want = chunk.min(end - next) as u32;
            let pending = session.reserve().await?.send(Op::Read {
                handle: handle.0.clone(),
                offset: next,
                len: want,
            })?;
            queue.push_back((next, want, pending));
            next += u64::from(want);
        }
        let Some((at, want, pending)) = queue.pop_front() else {
            return Ok(out);
        };
        match check_read(pending.wait().await?, want)? {
            // The file ends here; later requests are beyond it.
            None => return Ok(out),
            Some(data) if data.is_empty() => {
                return Err(Error::Protocol(ProtocolError::NoProgress));
            }
            Some(data) => {
                out.extend_from_slice(&data);
                let got = data.len() as u32;
                if got < want {
                    // A short reply: ask again for what is missing, ahead of
                    // everything that was sent after it.
                    let (at, want) = (at + u64::from(got), want - got);
                    let pending = session.reserve().await?.send(Op::Read {
                        handle: handle.0.clone(),
                        offset: at,
                        len: want,
                    })?;
                    queue.push_front((at, want, pending));
                }
            }
        }
    }
}

/// Writes `data` at `offset` with many requests in flight. See
/// [`File::write_all_at`].
pub(super) async fn write_all(
    session: &Session,
    handle: &Handle,
    offset: u64,
    mut data: Bytes,
) -> Result<(), Error> {
    offset
        .checked_add(data.len() as u64)
        .ok_or_else(|| invalid("range passes the largest offset"))?;
    let chunk = session.write_chunk(handle.0.len());
    let depth = session.pipeline_depth();
    let mut at = offset;
    let mut queue: VecDeque<PendingReply> = VecDeque::new();
    while !data.is_empty() || !queue.is_empty() {
        while !data.is_empty() && queue.len() < depth {
            let piece = data.split_to(chunk.min(data.len()));
            let n = piece.len() as u64;
            let pending = session.reserve().await?.send(Op::Write {
                handle: handle.0.clone(),
                offset: at,
                data: piece,
            })?;
            queue.push_back(pending);
            at += n;
        }
        if let Some(pending) = queue.pop_front() {
            check_ok(pending.wait().await?)?;
        }
    }
    Ok(())
}
