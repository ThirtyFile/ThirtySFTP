//! The protocol loop.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use super::config::ServerConfig;
use super::ext::{KNOWN, extended, file_of};
use super::filesystem::{Filesystem, FsError};
use super::longname::format_long_name;
use super::reply::{
    Outgoing, ReplyGuard, bad_request, eof_reply, invalid_handle, name_reply, ok_reply,
    status_reply,
};
use super::table::{Ctx, DirState, Kind, Order, Table, handle_key, lock};
use crate::codec::{self, FrameReader};
use crate::error::{Error, ProtocolError};
use crate::extensions as ext;
use crate::wire::{
    ClientPacket, Encode, Extension, Hello, Op, Reader, Reply, Request, ServerPacket,
};

/// The handles a request names, in order.
fn handles_of(op: &Op) -> Result<Vec<Bytes>, FsError> {
    Ok(match op {
        Op::Read { handle, .. }
        | Op::Write { handle, .. }
        | Op::Fstat { handle }
        | Op::FsetStat { handle, .. }
        | Op::ReadDir { handle } => vec![handle.clone()],
        Op::Extended { name, data } => {
            let mut r = Reader::new(data.clone());
            match &name[..] {
                n if n == ext::FSYNC.as_bytes() || n == ext::FSTATVFS.as_bytes() => {
                    vec![r.string().map_err(|_| bad_request())?]
                }
                n if n == ext::COPY_DATA.as_bytes() => {
                    let source = r.string().map_err(|_| bad_request())?;
                    r.u64().map_err(|_| bad_request())?;
                    r.u64().map_err(|_| bad_request())?;
                    let destination = r.string().map_err(|_| bad_request())?;
                    vec![source, destination]
                }
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    })
}

/// Runs one request that is not a close.
async fn exec<F: Filesystem>(ctx: &Ctx<F>, op: Op, kinds: Vec<Kind<F>>) -> Result<Reply, FsError> {
    let fs = &*ctx.fs;
    match op {
        Op::Open { path, flags, attrs } => {
            let reservation = ctx.reserve()?;
            let file = fs.open(&path, flags, &attrs).await?;
            let handle = ctx.insert(&reservation, Kind::File(Arc::new(file)));
            Ok(Reply::Handle(handle))
        }
        Op::OpenDir { path } => {
            let reservation = ctx.reserve()?;
            let dir = fs.open_dir(&path).await?;
            let state = DirState {
                dir,
                pending: Mutex::new(VecDeque::new()),
                finished: AtomicBool::new(false),
            };
            let handle = ctx.insert(&reservation, Kind::Dir(Arc::new(state)));
            Ok(Reply::Handle(handle))
        }
        Op::Read { offset, len, .. } => {
            let file = file_of(&kinds, 0)?;
            let len = len.min(ctx.cfg.max_read_length);
            if len == 0 {
                return Ok(Reply::Data(Bytes::new()));
            }
            Ok(match fs.read(file, offset, len).await? {
                Some(data) if !data.is_empty() => {
                    Reply::Data(data.slice(..data.len().min(len as usize)))
                }
                _ => eof_reply(),
            })
        }
        Op::Write { offset, data, .. } => {
            let file = file_of(&kinds, 0)?;
            if data.len() > ctx.cfg.max_write_length as usize {
                return Err(FsError::failure("write too long"));
            }
            fs.write(file, offset, data).await?;
            Ok(ok_reply())
        }
        Op::Fstat { .. } => Ok(Reply::Attrs(fs.fstat(file_of(&kinds, 0)?).await?)),
        Op::FsetStat { attrs, .. } => {
            fs.fsetstat(file_of(&kinds, 0)?, &attrs).await?;
            Ok(ok_reply())
        }
        Op::Stat { path } => Ok(Reply::Attrs(fs.stat(&path).await?)),
        Op::Lstat { path } => Ok(Reply::Attrs(fs.lstat(&path).await?)),
        Op::SetStat { path, attrs } => {
            fs.setstat(&path, &attrs).await?;
            Ok(ok_reply())
        }
        Op::ReadDir { .. } => {
            let Some(Kind::Dir(dir)) = kinds.first() else {
                return Err(invalid_handle());
            };
            read_dir(ctx, dir).await
        }
        Op::Remove { path } => {
            fs.remove(&path).await?;
            Ok(ok_reply())
        }
        Op::MkDir { path, attrs } => {
            fs.mkdir(&path, &attrs).await?;
            Ok(ok_reply())
        }
        Op::RmDir { path } => {
            fs.rmdir(&path).await?;
            Ok(ok_reply())
        }
        Op::RealPath { path } => Ok(name_reply(fs.realpath(&path).await?)),
        Op::Rename { from, to } => {
            fs.rename(&from, &to).await?;
            Ok(ok_reply())
        }
        Op::ReadLink { path } => Ok(name_reply(fs.read_link(&path).await?)),
        Op::Symlink { target, link } => {
            fs.symlink(&target, &link).await?;
            Ok(ok_reply())
        }
        Op::Extended { name, data } => extended(ctx, &name, data, &kinds).await,
        Op::Close { .. } | Op::Unknown { .. } => Err(FsError::unsupported()),
    }
}

/// Answers a directory read from what is buffered, asking the file system
/// for more only when the buffer is empty.
async fn read_dir<F: Filesystem>(ctx: &Ctx<F>, dir: &DirState<F>) -> Result<Reply, FsError> {
    loop {
        {
            let mut pending = lock(&dir.pending);
            if !pending.is_empty() {
                let mut batch = Vec::new();
                let mut size = 0usize;
                while batch.len() < ctx.cfg.max_dir_entries
                    && size < ctx.cfg.max_packet as usize / 2
                    && let Some(mut entry) = pending.pop_front()
                {
                    if entry.long_name.is_empty() {
                        entry.long_name = format_long_name(&entry.name, &entry.attrs);
                    }
                    size += entry.name.len() + entry.long_name.len() + 64;
                    batch.push(entry);
                }
                return Ok(Reply::Name(batch));
            }
        }
        if dir.finished.load(Ordering::Acquire) {
            return Ok(eof_reply());
        }
        match ctx.fs.read_dir(&dir.dir).await? {
            Some(batch) if !batch.is_empty() => lock(&dir.pending).extend(batch),
            _ => dir.finished.store(true, Ordering::Release),
        }
    }
}

/// Closes the handle a `CLOSE` request names, once earlier requests on it
/// are done.
async fn close<F: Filesystem>(
    ctx: &Ctx<F>,
    kind: Kind<F>,
    tail: Option<oneshot::Receiver<()>>,
) -> Result<Reply, FsError> {
    if let Some(tail) = tail {
        let _ = tail.await;
    }
    match kind {
        Kind::File(file) => {
            let file = Arc::try_unwrap(file).map_err(|_| FsError::failure("handle busy"))?;
            ctx.fs.close_file(file).await?;
        }
        Kind::Dir(dir) => {
            let dir = Arc::try_unwrap(dir).map_err(|_| FsError::failure("handle busy"))?;
            ctx.fs.close_dir(dir.dir).await?;
        }
    }
    Ok(ok_reply())
}

/// Aborts a task when dropped.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Writes frames; reports the first write failure and stops.
async fn write_loop<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut frames: mpsc::UnboundedReceiver<Outgoing>,
    failed: oneshot::Sender<io::Error>,
) {
    while let Some((frame, permit)) = frames.recv().await {
        // A reply counts as pending until it is written, so a slow client
        // cannot make the queue grow without bound.
        let mut permits = vec![permit];
        let written = async {
            writer.write_all(&frame).await?;
            while let Ok((more, permit)) = frames.try_recv() {
                permits.push(permit);
                writer.write_all(&more).await?;
            }
            writer.flush().await
        }
        .await;
        if let Err(e) = written {
            let _ = failed.send(e);
            return;
        }
    }
    let _ = writer.shutdown().await;
}

fn timed_out(what: &'static str) -> Error {
    io::Error::new(io::ErrorKind::TimedOut, what).into()
}

enum Event {
    /// Nothing arrived and nothing was being served for the idle limit.
    Idle,
    Frame(Result<Option<Bytes>, codec::Error>),
    WriteFailed(io::Error),
}

/// The next packet from the client, or the writer's failure, whichever comes
/// first. Cancel safe.
async fn next_event<R: AsyncRead + Unpin>(
    reader: &mut FrameReader<R>,
    failed: &mut Option<oneshot::Receiver<io::Error>>,
) -> Event {
    let mut read = Box::pin(reader.read_frame());
    std::future::poll_fn(|cx| {
        if let Poll::Ready(frame) = read.as_mut().poll(cx) {
            return Poll::Ready(Event::Frame(frame));
        }
        if let Some(rx) = failed.as_mut() {
            match Pin::new(rx).poll(cx) {
                Poll::Ready(Ok(e)) => return Poll::Ready(Event::WriteFailed(e)),
                // The writer ended without failing.
                Poll::Ready(Err(_)) => *failed = None,
                Poll::Pending => {}
            }
        }
        Poll::Pending
    })
    .await
}

/// Serves one SFTP connection until the client closes it.
///
/// Reads requests from `reader`, runs them on `fs`, and writes replies to
/// `writer`. Each request runs in its own task, so a slow file system call
/// delays only its own reply; requests on one handle keep their order. The
/// limits in `config` bound memory and work per connection.
///
/// Returns `Ok(())` when the client ends the stream between packets. Other
/// endings are errors: an I/O failure, or a protocol violation such as a
/// packet over [`max_packet`](ServerConfig::max_packet), a request before
/// the handshake, a second handshake, or one of the time limits in the
/// configuration running out. Requests that fail individually,
/// including malformed ones whose id can be read, get a status reply and the
/// connection continues. Handles still open at the end are closed.
///
/// Dropping the returned future ends the connection at once.
pub async fn serve<R, W, F>(
    mut reader: R,
    mut writer: W,
    fs: Arc<F>,
    config: ServerConfig,
) -> Result<(), Error>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send + 'static,
    F: Filesystem,
{
    let cfg = config.effective();
    let mut frames_in = FrameReader::new(&mut reader, cfg.max_packet);

    // Handshake.
    let first = match cfg.handshake_timeout {
        Some(limit) => tokio::time::timeout(limit, frames_in.read_frame())
            .await
            .map_err(|_| timed_out("no handshake from the client"))?,
        None => frames_in.read_frame().await,
    };
    let payload = first?.ok_or(Error::Disconnected)?;
    match ClientPacket::decode(payload) {
        Ok(ClientPacket::Init(hello)) if hello.version >= 3 => {}
        Ok(ClientPacket::Init(hello)) => {
            return Err(Error::Protocol(ProtocolError::UnsupportedVersion(
                hello.version,
            )));
        }
        Ok(ClientPacket::Request(_)) => {
            return Err(Error::Protocol(ProtocolError::UnexpectedHandshake));
        }
        Err(e) => return Err(Error::Protocol(ProtocolError::Decode(e))),
    }
    let mut announced: Vec<&'static str> = vec![ext::LIMITS];
    announced.extend(
        fs.extensions()
            .iter()
            .copied()
            .filter(|n| KNOWN.contains(n) && *n != ext::LIMITS),
    );
    let mut out = BytesMut::new();
    ServerPacket::Version(Hello {
        version: 3,
        extensions: announced
            .iter()
            .map(|n| Extension {
                name: Bytes::from_static(n.as_bytes()),
                // OpenSSH's client only uses `statvfs` when it is announced
                // as version 2; every other extension here is version 1.
                data: Bytes::from_static(if *n == ext::STATVFS || *n == ext::FSTATVFS {
                    b"2"
                } else {
                    b"1"
                }),
            })
            .collect(),
    })
    .encode_frame(&mut out)
    .map_err(Error::Encode)?;
    writer.write_all(&out).await?;
    writer.flush().await?;

    let ctx = Arc::new(Ctx {
        fs,
        cfg,
        table: Mutex::new(Table {
            entries: HashMap::new(),
            opening: 0,
            next: 0,
        }),
        announced,
    });
    let slots = Arc::new(Semaphore::new(ctx.cfg.max_pending));
    let (frames_tx, frames_rx) = mpsc::unbounded_channel();
    let (failed_tx, failed_rx) = oneshot::channel();
    let writer_task = AbortOnDrop(tokio::spawn(write_loop(writer, frames_rx, failed_tx)));
    let mut failed = Some(failed_rx);
    let mut tasks: JoinSet<()> = JoinSet::new();

    let result = loop {
        // Stop reading while the limit of unfinished requests is reached.
        let Ok(permit) = slots.clone().acquire_owned().await else {
            break Err(Error::Closed);
        };
        while tasks.try_join_next().is_some() {}
        let event = match ctx.cfg.idle_timeout {
            None => next_event(&mut frames_in, &mut failed).await,
            Some(limit) => loop {
                match tokio::time::timeout(limit, next_event(&mut frames_in, &mut failed)).await {
                    Ok(event) => break Some(event),
                    // Nothing arrived. That is only idleness if nothing is
                    // being served either: every slot but ours is free.
                    Err(_) if slots.available_permits() + 1 >= ctx.cfg.max_pending => {
                        break None;
                    }
                    Err(_) => {}
                }
            }
            .unwrap_or_else(|| Event::Idle),
        };
        let payload = match event {
            Event::Idle => break Err(timed_out("the connection was idle")),
            Event::Frame(Ok(Some(payload))) => payload,
            Event::Frame(Ok(None)) => break Ok(()),
            Event::Frame(Err(e)) => break Err(e.into()),
            Event::WriteFailed(e) => break Err(e.into()),
        };
        let guard = |id| ReplyGuard {
            id,
            frames: frames_tx.clone(),
            permit: Some(permit),
        };
        let request = match ClientPacket::decode(payload.clone()) {
            Ok(ClientPacket::Request(request)) => request,
            Ok(ClientPacket::Init(_)) => {
                break Err(Error::Protocol(ProtocolError::UnexpectedHandshake));
            }
            Err(e) => {
                // The frame is intact, so the connection can go on if the
                // request id is readable.
                if payload.len() < 5 {
                    break Err(Error::Protocol(ProtocolError::Decode(e)));
                }
                let id = u32::from_be_bytes([payload[1], payload[2], payload[3], payload[4]]);
                guard(id).send(status_reply(&bad_request()));
                continue;
            }
        };
        dispatch(&ctx, &mut tasks, guard(request.id), request);
    };

    // Let running requests finish and their replies be written.
    drop(frames_tx);
    while tasks.join_next().await.is_some() {}
    let mut writer_task = writer_task;
    let _ = (&mut writer_task.0).await;

    // Close what the client left open.
    let leftovers: Vec<_> = lock(&ctx.table).entries.drain().collect();
    for (_, entry) in leftovers {
        match entry.kind {
            Kind::File(file) => {
                if let Ok(file) = Arc::try_unwrap(file) {
                    let _ = ctx.fs.close_file(file).await;
                }
            }
            Kind::Dir(dir) => {
                if let Ok(dir) = Arc::try_unwrap(dir) {
                    let _ = ctx.fs.close_dir(dir.dir).await;
                }
            }
        }
    }
    result
}

/// Starts the task for one request. Takes its place in the order of every
/// handle it names before returning, so requests on a handle run in the
/// order they arrived.
fn dispatch<F: Filesystem>(
    ctx: &Arc<Ctx<F>>,
    tasks: &mut JoinSet<()>,
    guard: ReplyGuard,
    request: Request,
) {
    let op = request.op;
    let ctx = ctx.clone();

    if let Op::Close { handle } = &op {
        let removed = handle_key(handle).and_then(|key| lock(&ctx.table).entries.remove(&key));
        let Some(entry) = removed else {
            guard.send(status_reply(&invalid_handle()));
            return;
        };
        tasks.spawn(async move {
            let reply = close(&ctx, entry.kind, entry.tail)
                .await
                .unwrap_or_else(|e| status_reply(&e));
            guard.send(reply);
        });
        return;
    }

    let handles = match handles_of(&op) {
        Ok(handles) => handles,
        Err(e) => {
            guard.send(status_reply(&e));
            return;
        }
    };
    let mut kinds = Vec::with_capacity(handles.len());
    let mut orders: Vec<Order> = Vec::new();
    let mut seen: Vec<(&Bytes, Kind<F>)> = Vec::new();
    for handle in &handles {
        // A handle named twice takes one place in the order, or the request
        // would wait for itself.
        if let Some((_, kind)) = seen.iter().find(|(h, _)| *h == handle) {
            kinds.push(kind.clone());
            continue;
        }
        let Some((kind, order)) = ctx.lookup(handle) else {
            guard.send(status_reply(&invalid_handle()));
            return;
        };
        seen.push((handle, kind.clone()));
        kinds.push(kind);
        orders.push(order);
    }
    tasks.spawn(async move {
        for order in &mut orders {
            order.wait().await;
        }
        let reply = exec(&ctx, op, kinds)
            .await
            .unwrap_or_else(|e| status_reply(&e));
        // Only now may the next request on these handles start.
        drop(orders);
        guard.send(reply);
    });
}
