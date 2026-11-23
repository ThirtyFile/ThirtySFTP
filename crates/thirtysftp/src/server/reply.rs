//! Replies: the common ones, and the guard that makes sure every request
//! gets one.

use bytes::{Bytes, BytesMut};
use tokio::sync::{OwnedSemaphorePermit, mpsc};

use super::filesystem::FsError;
use crate::wire::{Attributes, Encode, NameEntry, Reply, Response, ServerPacket, StatusCode};

pub(super) type Outgoing = (Bytes, OwnedSemaphorePermit);

/// Sends the reply to one request, or a failure if the task dies first.
pub(super) struct ReplyGuard {
    pub(super) id: u32,
    pub(super) frames: mpsc::UnboundedSender<Outgoing>,
    pub(super) permit: Option<OwnedSemaphorePermit>,
}

impl ReplyGuard {
    pub(super) fn send(mut self, reply: Reply) {
        self.transmit(reply);
    }

    pub(super) fn transmit(&mut self, reply: Reply) {
        let Some(permit) = self.permit.take() else {
            return;
        };
        let mut frame = BytesMut::new();
        let mut packet = ServerPacket::Response(Response { id: self.id, reply });
        if packet.encode_frame(&mut frame).is_err() {
            frame.clear();
            packet = ServerPacket::Response(Response {
                id: self.id,
                reply: status_reply(&FsError::failure("reply too large")),
            });
            // A short status always fits.
            let _ = packet.encode_frame(&mut frame);
        }
        // The writer is gone only when the connection is ending.
        let _ = self.frames.send((frame.freeze(), permit));
    }
}

impl Drop for ReplyGuard {
    fn drop(&mut self) {
        // Reached unsent only if the task panicked or was cancelled.
        self.transmit(status_reply(&FsError::failure("internal error")));
    }
}

pub(super) fn status_reply(e: &FsError) -> Reply {
    Reply::Status {
        code: e.code,
        message: Bytes::copy_from_slice(e.message.as_bytes()),
        language: Bytes::new(),
    }
}

pub(super) fn ok_reply() -> Reply {
    Reply::Status {
        code: StatusCode::Ok,
        message: Bytes::new(),
        language: Bytes::new(),
    }
}

pub(super) fn eof_reply() -> Reply {
    Reply::Status {
        code: StatusCode::Eof,
        message: Bytes::new(),
        language: Bytes::new(),
    }
}

pub(super) fn bad_request() -> FsError {
    FsError::new(StatusCode::BadMessage, "malformed request")
}

pub(super) fn invalid_handle() -> FsError {
    FsError::failure("invalid handle")
}

pub(super) fn name_reply(name: Bytes) -> Reply {
    Reply::Name(vec![NameEntry {
        long_name: name.clone(),
        name,
        attrs: Attributes::default(),
    }])
}
