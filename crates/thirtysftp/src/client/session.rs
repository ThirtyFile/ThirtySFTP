//! The request engine: handshake, request ids, reply matching.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio::task::AbortHandle;

use super::ext::limits_from_reply;
use crate::codec::{DEFAULT_MAX_FRAME, FrameReader};
use crate::error::{Error, ProtocolError};
use crate::extensions::{LIMITS, Limits};
use crate::wire::{ClientPacket, Encode, Extension, Hello, Op, Reply, Request, ServerPacket};

/// Request size when the server states no limits.
const AUTO_CHUNK: u64 = 32 * 1024;
/// Request size when it does: as much as it allows, up to this.
const AUTO_CHUNK_WITH_LIMITS: u64 = 128 * 1024;

/// The protocol version this client speaks.
const VERSION: u32 = 3;

/// Settings of a [`Session`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Config {
    /// Most requests that may wait for a reply at once. Further requests
    /// wait for a free slot before they are sent. Values below 1 count as 1.
    pub max_in_flight: usize,
    /// Longest reply packet accepted, in bytes. A longer one is a protocol
    /// violation and ends the session.
    pub max_frame: u32,
    /// Bytes per read or write request made by [`File`](super::File) and the
    /// `fs` functions. `None` (the default) lets the session choose: 32 KiB, as
    /// OpenSSH does, unless the server states its limits with
    /// `limits@openssh.com`, in which case the most the server allows, up to
    /// 128 KiB. A value set here is also kept within the server's limits.
    pub chunk_size: Option<u32>,
    /// Requests a single file transfer keeps in flight. A value above
    /// `max_in_flight` is harmless: the extra ones wait for a slot.
    pub pipeline_depth: usize,
}

impl Config {
    /// Sets [`max_in_flight`](Self::max_in_flight).
    pub fn with_max_in_flight(mut self, max: usize) -> Self {
        self.max_in_flight = max;
        self
    }

    /// Sets [`chunk_size`](Self::chunk_size).
    pub fn with_chunk_size(mut self, size: u32) -> Self {
        self.chunk_size = Some(size);
        self
    }

    /// Sets [`pipeline_depth`](Self::pipeline_depth).
    pub fn with_pipeline_depth(mut self, depth: usize) -> Self {
        self.pipeline_depth = depth;
        self
    }

    /// Sets [`max_frame`](Self::max_frame).
    pub fn with_max_frame(mut self, max: u32) -> Self {
        self.max_frame = max;
        self
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_in_flight: 64,
            max_frame: DEFAULT_MAX_FRAME,
            chunk_size: None,
            pipeline_depth: 64,
        }
    }
}

/// An open remote file or directory, as the server names it. The bytes are
/// opaque. Close it with [`Session::close_handle`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Handle(pub(crate) Bytes);

impl Handle {
    /// A handle with the given bytes, for example one saved earlier. The
    /// server rejects handles it did not issue.
    pub fn from_bytes(bytes: impl Into<Bytes>) -> Self {
        Self(bytes.into())
    }

    /// The handle as the server sent it.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// A request waiting for its reply. The permit is released when the reply
/// arrives (or the session fails), not when the caller gives up waiting.
struct Pending {
    reply: oneshot::Sender<Reply>,
    _permit: OwnedSemaphorePermit,
}

struct State {
    pending: HashMap<u32, Pending>,
    next_id: u32,
    /// Set once; the session never recovers.
    failed: Option<Error>,
    /// Frames for the writer task. Taken when the session fails, which also
    /// ends the writer.
    frames: Option<mpsc::UnboundedSender<Bytes>>,
}

struct Shared {
    state: Mutex<State>,
    slots: Arc<Semaphore>,
    /// Signalled when the session fails, so that a write stuck on a peer that
    /// has stopped reading is abandoned instead of holding the stream forever.
    ended: Notify,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // Nothing run under the lock can panic, but stay usable regardless.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Ends the session: every waiter is woken, every later request fails.
    /// Only the first call has any effect.
    fn fail(&self, error: Error) {
        let (pending, frames) = {
            let mut state = self.lock();
            if state.failed.is_some() {
                return;
            }
            state.failed = Some(error);
            (std::mem::take(&mut state.pending), state.frames.take())
        };
        self.slots.close();
        self.ended.notify_one();
        drop(pending);
        drop(frames);
    }

    fn has_failed(&self) -> bool {
        self.lock().failed.is_some()
    }

    fn failure(&self) -> Error {
        self.lock().failed.clone().unwrap_or(Error::Closed)
    }

    /// Hands a reply to the request it answers.
    fn deliver(&self, id: u32, reply: Reply) -> Result<(), Error> {
        let pending = self.lock().pending.remove(&id);
        match pending {
            Some(p) => {
                // The caller may have stopped waiting; that is fine.
                let _ = p.reply.send(reply);
                Ok(())
            }
            None => Err(Error::Protocol(ProtocolError::UnknownRequestId(id))),
        }
    }
}

struct Inner {
    shared: Arc<Shared>,
    reader: AbortHandle,
    extensions: Vec<Extension>,
    chunk_size: Option<u32>,
    pipeline_depth: usize,
    limits: OnceLock<Limits>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.shared.fail(Error::Closed);
        self.reader.abort();
    }
}

/// A connection to an SFTP server.
///
/// Cloning is cheap and gives another handle to the same connection. The
/// connection ends when the last clone is dropped, or earlier on
/// [`close`](Self::close) or a failure.
#[derive(Clone)]
pub struct Session {
    inner: Arc<Inner>,
}

impl Session {
    /// Performs the handshake on a pair of streams and starts the session.
    ///
    /// Fails if the server does not answer with protocol version 3. The
    /// handshake waits for the server without a time limit; wrap the call in
    /// `tokio::time::timeout` to bound it.
    pub async fn connect<R, W>(reader: R, mut writer: W, config: Config) -> Result<Self, Error>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let mut out = BytesMut::new();
        ClientPacket::Init(Hello {
            version: VERSION,
            extensions: Vec::new(),
        })
        .encode_frame(&mut out)
        .map_err(Error::Encode)?;
        writer.write_all(&out).await?;
        writer.flush().await?;

        let mut reader = FrameReader::new(reader, config.max_frame);
        let payload = reader.read_frame().await?.ok_or(Error::Disconnected)?;
        let hello = match ServerPacket::decode(payload) {
            Ok(ServerPacket::Version(hello)) => hello,
            Ok(ServerPacket::Response(_)) => {
                return Err(Error::Protocol(ProtocolError::UnexpectedReply("version")));
            }
            Err(e) => return Err(Error::Protocol(ProtocolError::Decode(e))),
        };
        if hello.version != VERSION {
            return Err(Error::Protocol(ProtocolError::UnsupportedVersion(
                hello.version,
            )));
        }

        let max_in_flight = config.max_in_flight.clamp(1, Semaphore::MAX_PERMITS);
        let (frames, queue) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                pending: HashMap::new(),
                next_id: 0,
                failed: None,
                frames: Some(frames),
            }),
            slots: Arc::new(Semaphore::new(max_in_flight)),
            ended: Notify::new(),
        });
        tokio::spawn(write_loop(writer, queue, shared.clone()));
        let reader = tokio::spawn(read_loop(reader, shared.clone())).abort_handle();
        let session = Self {
            inner: Arc::new(Inner {
                shared,
                reader,
                extensions: hello.extensions,
                chunk_size: config.chunk_size.map(|size| size.max(1)),
                pipeline_depth: config.pipeline_depth.max(1),
                limits: OnceLock::new(),
            }),
        };
        // Use the server's own limits when it states them. A failure here
        // shows up again on the first real request, so it is not reported.
        if session.supports(LIMITS)
            && let Ok(limits) = session.fetch_limits().await
        {
            let _ = session.inner.limits.set(limits);
        }
        Ok(session)
    }

    /// Whether the server announced the extension `name` in its handshake.
    pub fn supports(&self, name: &str) -> bool {
        self.extension(name).is_some()
    }

    /// The limits the server stated, if it supports `limits@openssh.com`.
    pub fn known_limits(&self) -> Option<&Limits> {
        self.inner.limits.get()
    }

    /// The request size before the server's limits are applied.
    fn base_chunk(&self) -> u64 {
        match self.inner.chunk_size {
            Some(size) => u64::from(size),
            // Without limits there is nothing to say how much the server
            // takes; 32 KiB is what OpenSSH uses. With them, use what the
            // server allows, up to the size past which measurements stop
            // improving.
            None if self.inner.limits.get().is_some() => AUTO_CHUNK_WITH_LIMITS,
            None => AUTO_CHUNK,
        }
    }

    /// Bytes per read request for transfers.
    pub(crate) fn read_chunk(&self) -> u32 {
        let mut chunk = self.base_chunk();
        if let Some(l) = self.inner.limits.get() {
            if l.max_read_length > 0 {
                chunk = chunk.min(l.max_read_length);
            }
            if l.max_packet_length > 0 {
                chunk = chunk.min(l.max_packet_length.saturating_sub(64));
            }
        }
        u32::try_from(chunk.max(1)).unwrap_or(u32::MAX)
    }

    /// Bytes per write request for transfers on a handle of `handle_len`
    /// bytes.
    pub(crate) fn write_chunk(&self, handle_len: usize) -> usize {
        let mut chunk = self.base_chunk();
        if let Some(l) = self.inner.limits.get() {
            if l.max_write_length > 0 {
                chunk = chunk.min(l.max_write_length);
            }
            if l.max_packet_length > 0 {
                let overhead = 64 + handle_len as u64;
                chunk = chunk.min(l.max_packet_length.saturating_sub(overhead));
            }
        }
        usize::try_from(chunk.max(1)).unwrap_or(usize::MAX)
    }

    /// Requests a transfer keeps in flight.
    pub(crate) fn pipeline_depth(&self) -> usize {
        self.inner.pipeline_depth
    }

    /// The extensions the server announced in its handshake.
    pub fn extensions(&self) -> &[Extension] {
        &self.inner.extensions
    }

    /// The data of the extension `name`, if the server announced it.
    pub fn extension(&self, name: &str) -> Option<&Bytes> {
        self.inner
            .extensions
            .iter()
            .find(|e| e.name == name.as_bytes())
            .map(|e| &e.data)
    }

    /// Ends the session. Requests still waiting fail with [`Error::Closed`].
    /// Frames not yet written are dropped, a write in progress is abandoned,
    /// and the stream is released; if the writer was between frames the write
    /// half is shut down first, so the server sees an orderly end.
    pub fn close(&self) {
        self.inner.shared.fail(Error::Closed);
        self.inner.reader.abort();
    }

    /// Sends one request and waits for its reply.
    ///
    /// Cancel safe; see the [module documentation](super).
    pub(crate) async fn request(&self, op: Op) -> Result<Reply, Error> {
        self.reserve().await?.send(op)?.wait().await
    }

    /// Waits for a free request slot.
    ///
    /// Cancel safe. Sending on the returned [`Slot`] needs no waiting, so a
    /// request is either not registered at all or registered and sent.
    pub(crate) async fn reserve(&self) -> Result<Slot, Error> {
        let shared = &self.inner.shared;
        let permit = shared
            .slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| shared.failure())?;
        Ok(Slot {
            shared: shared.clone(),
            permit,
        })
    }

    pub(super) async fn fetch_limits(&self) -> Result<Limits, Error> {
        let reply = self
            .request(Op::Extended {
                name: Bytes::from_static(LIMITS.as_bytes()),
                data: Bytes::new(),
            })
            .await?;
        limits_from_reply(reply)
    }
}

/// A reserved request slot.
pub(crate) struct Slot {
    shared: Arc<Shared>,
    permit: OwnedSemaphorePermit,
}

impl Slot {
    /// Registers and sends a request. Never waits.
    pub(crate) fn send(self, op: Op) -> Result<PendingReply, Error> {
        let shared = self.shared;
        let (reply_tx, reply_rx) = oneshot::channel();
        let id = {
            let mut state = shared.lock();
            if let Some(error) = &state.failed {
                return Err(error.clone());
            }
            // Ids of requests still waiting are never handed out again. At
            // most `max_in_flight` are taken, so this terminates.
            let id = loop {
                let id = state.next_id;
                state.next_id = id.wrapping_add(1);
                if !state.pending.contains_key(&id) {
                    break id;
                }
            };
            state.pending.insert(
                id,
                Pending {
                    reply: reply_tx,
                    _permit: self.permit,
                },
            );
            id
        };

        let mut frame = BytesMut::new();
        if let Err(e) = ClientPacket::Request(Request { id, op }).encode_frame(&mut frame) {
            shared.lock().pending.remove(&id);
            return Err(Error::Encode(e));
        }
        let sent = shared
            .lock()
            .frames
            .as_ref()
            .is_some_and(|frames| frames.send(frame.freeze()).is_ok());
        if !sent {
            shared.lock().pending.remove(&id);
            return Err(shared.failure());
        }
        Ok(PendingReply {
            rx: reply_rx,
            shared,
        })
    }
}

/// A request that has been sent and is waiting for its reply.
///
/// Dropping it does not cancel the request: the id and the slot stay
/// reserved until the reply arrives.
pub(crate) struct PendingReply {
    rx: oneshot::Receiver<Reply>,
    shared: Arc<Shared>,
}

impl PendingReply {
    /// Waits for the reply. Cancel safe.
    pub(crate) async fn wait(mut self) -> Result<Reply, Error> {
        std::future::poll_fn(|cx| self.poll_reply(cx)).await
    }

    /// Polls for the reply. Must not be called again after it returned
    /// `Ready`.
    pub(crate) fn poll_reply(&mut self, cx: &mut Context<'_>) -> Poll<Result<Reply, Error>> {
        match std::pin::Pin::new(&mut self.rx).poll(cx) {
            Poll::Ready(Ok(reply)) => Poll::Ready(Ok(reply)),
            Poll::Ready(Err(_)) => Poll::Ready(Err(self.shared.failure())),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Owns the read half: decodes replies and hands them to their requests.
async fn read_loop<R: AsyncRead + Unpin>(mut reader: FrameReader<R>, shared: Arc<Shared>) {
    let error = loop {
        let payload = match reader.read_frame().await {
            Ok(Some(payload)) => payload,
            Ok(None) => break Error::Disconnected,
            Err(e) => break e.into(),
        };
        let response = match ServerPacket::decode(payload) {
            Ok(ServerPacket::Response(response)) => response,
            Ok(ServerPacket::Version(_)) => {
                break Error::Protocol(ProtocolError::UnexpectedHandshake);
            }
            Err(e) => break Error::Protocol(ProtocolError::Decode(e)),
        };
        if let Err(e) = shared.deliver(response.id, response.reply) {
            break e;
        }
    };
    shared.fail(error);
}

/// Owns the write half: writes queued frames, several per flush when they
/// are waiting. Ends when the session fails or is closed.
async fn write_loop<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut queue: mpsc::UnboundedReceiver<Bytes>,
    shared: Arc<Shared>,
) {
    while let Some(frame) = queue.recv().await {
        // Once the session has ended, nothing more is sent.
        if shared.has_failed() {
            break;
        }
        let write = async {
            writer.write_all(&frame).await?;
            while let Ok(more) = queue.try_recv() {
                writer.write_all(&more).await?;
            }
            writer.flush().await
        };
        let mut write = std::pin::pin!(write);
        let mut ended = std::pin::pin!(shared.ended.notified());
        // The write, or the end of the session, whichever comes first.
        let written = std::future::poll_fn(|cx| {
            if let Poll::Ready(result) = write.as_mut().poll(cx) {
                return Poll::Ready(Some(result));
            }
            match ended.as_mut().poll(cx) {
                Poll::Ready(()) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            }
        })
        .await;
        match written {
            Some(Ok(())) => {}
            Some(Err(e)) => {
                shared.fail(e.into());
                return;
            }
            // Abandoned in the middle of a frame: the stream cannot be ended
            // in an orderly way, so it is only dropped.
            None => return,
        }
    }
    let _ = writer.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::StatusCode;
    use std::time::Duration;
    use tokio::io::{DuplexStream, ReadHalf, WriteHalf, duplex, split};
    use tokio::time::timeout;

    struct Server {
        reader: FrameReader<ReadHalf<DuplexStream>>,
        writer: WriteHalf<DuplexStream>,
    }

    impl Server {
        async fn request(&mut self) -> Request {
            let payload = self.reader.read_frame().await.unwrap().unwrap();
            match ClientPacket::decode(payload).unwrap() {
                ClientPacket::Request(r) => r,
                other => panic!("unexpected {other:?}"),
            }
        }

        async fn ok(&mut self, id: u32) {
            let mut out = BytesMut::new();
            crate::wire::ServerPacket::Response(crate::wire::Response {
                id,
                reply: Reply::Status {
                    code: StatusCode::Ok,
                    message: Bytes::new(),
                    language: Bytes::new(),
                },
            })
            .encode_frame(&mut out)
            .unwrap();
            self.writer.write_all(&out).await.unwrap();
        }
    }

    async fn pair(config: Config) -> (Session, Server) {
        let (client, server) = duplex(64 * 1024);
        let (cr, cw) = split(client);
        let (sr, mut sw) = split(server);
        let mut reader = FrameReader::new(sr, 1 << 16);
        let connect = tokio::spawn(Session::connect(cr, cw, config));
        reader.read_frame().await.unwrap().unwrap();
        let mut out = BytesMut::new();
        ServerPacket::Version(Hello {
            version: 3,
            extensions: vec![],
        })
        .encode_frame(&mut out)
        .unwrap();
        sw.write_all(&out).await.unwrap();
        let session = connect.await.unwrap().unwrap();
        (session, Server { reader, writer: sw })
    }

    fn op() -> Op {
        Op::Remove {
            path: Bytes::from_static(b"x"),
        }
    }

    #[tokio::test]
    async fn ids_wrap_around_at_the_top_of_the_range() {
        let (session, mut server) = pair(Config::default()).await;
        session.inner.shared.lock().next_id = u32::MAX;
        let a = tokio::spawn({
            let s = session.clone();
            async move { s.request(op()).await }
        });
        let first = server.request().await;
        assert_eq!(first.id, u32::MAX);
        server.ok(first.id).await;
        a.await.unwrap().unwrap();
        let b = tokio::spawn({
            let s = session.clone();
            async move { s.request(op()).await }
        });
        assert_eq!(server.request().await.id, 0);
        b.abort();
    }

    #[tokio::test]
    async fn an_id_still_waiting_is_skipped_when_the_counter_comes_around() {
        let (session, mut server) = pair(Config::default()).await;
        // Request 0 is sent and abandoned; its reply has not come yet.
        let abandoned = timeout(Duration::from_millis(20), session.request(op())).await;
        assert!(abandoned.is_err());
        let held = server.request().await.id;
        assert_eq!(held, 0);
        // Pretend the counter wrapped all the way around to the held id.
        session.inner.shared.lock().next_id = held;
        let s = session.clone();
        let next = tokio::spawn(async move { s.request(op()).await });
        let second = server.request().await;
        assert_ne!(second.id, held);
        // The late reply to the abandoned request must not reach `next`.
        server.ok(held).await;
        tokio::task::yield_now().await;
        assert!(!next.is_finished());
        server.ok(second.id).await;
        next.await.unwrap().unwrap();
    }
}
