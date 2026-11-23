//! Packet types and their encodings.

use std::ops::BitOr;

use bytes::{BufMut, Bytes, BytesMut};

use super::{Attributes, DecodeError, Encode, EncodeError, Reader, frame, put_string};

// Packet type bytes.
const INIT: u8 = 1;
const VERSION: u8 = 2;
const OPEN: u8 = 3;
const CLOSE: u8 = 4;
const READ: u8 = 5;
const WRITE: u8 = 6;
const LSTAT: u8 = 7;
const FSTAT: u8 = 8;
const SETSTAT: u8 = 9;
const FSETSTAT: u8 = 10;
const OPENDIR: u8 = 11;
const READDIR: u8 = 12;
const REMOVE: u8 = 13;
const MKDIR: u8 = 14;
const RMDIR: u8 = 15;
const REALPATH: u8 = 16;
const STAT: u8 = 17;
const RENAME: u8 = 18;
const READLINK: u8 = 19;
const SYMLINK: u8 = 20;
const STATUS: u8 = 101;
const HANDLE: u8 = 102;
const DATA: u8 = 103;
const NAME: u8 = 104;
const ATTRS: u8 = 105;
const EXTENDED: u8 = 200;
const EXTENDED_REPLY: u8 = 201;

/// A name and its data: an extension advertised in the handshake, or a
/// vendor attribute pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Extension {
    /// Extension name, for example `posix-rename@openssh.com`.
    pub name: Bytes,
    /// Extension data, for example the version string `1`.
    pub data: Bytes,
}

/// The handshake packet: `INIT` from the client, `VERSION` from the server.
/// Both carry a protocol version and a list of extensions that runs to the
/// end of the packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    /// Protocol version.
    pub version: u32,
    /// Extension name and data pairs.
    pub extensions: Vec<Extension>,
}

impl Hello {
    fn encode(&self, ty: u8, out: &mut BytesMut) -> Result<(), EncodeError> {
        out.put_u8(ty);
        out.put_u32(self.version);
        for e in &self.extensions {
            put_string(out, &e.name)?;
            put_string(out, &e.data)?;
        }
        Ok(())
    }

    fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        let version = r.u32()?;
        let mut extensions = Vec::new();
        while r.remaining() > 0 {
            extensions.push(Extension {
                name: r.string()?,
                data: r.string()?,
            });
        }
        Ok(Self {
            version,
            extensions,
        })
    }
}

/// Flags of an `OPEN` request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenFlags(pub u32);

impl OpenFlags {
    /// Open for reading.
    pub const READ: Self = Self(0x01);
    /// Open for writing.
    pub const WRITE: Self = Self(0x02);
    /// Writes append to the end of the file.
    pub const APPEND: Self = Self(0x04);
    /// Create the file if it does not exist.
    pub const CREATE: Self = Self(0x08);
    /// Truncate an existing file to zero length.
    pub const TRUNCATE: Self = Self(0x10);
    /// With `CREATE`, fail if the file exists.
    pub const EXCLUSIVE: Self = Self(0x20);

    /// Whether every flag in `other` is set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for OpenFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// Result code of a `STATUS` reply. Values this implementation does not know
/// are kept in [`Other`](Self::Other); [`from_u32`](Self::from_u32) never
/// produces `Other` for a known value, so codes compare reliably.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StatusCode {
    /// Success.
    Ok,
    /// End of file, or no more directory entries.
    Eof,
    /// The file does not exist.
    NoSuchFile,
    /// Permission denied.
    PermissionDenied,
    /// Generic failure.
    Failure,
    /// The request could not be parsed.
    BadMessage,
    /// There is no connection to the server.
    NoConnection,
    /// The connection was lost.
    ConnectionLost,
    /// The operation is not supported.
    OpUnsupported,
    /// A code outside the version 3 range.
    Other(u32),
}

impl StatusCode {
    /// Converts a wire value.
    pub const fn from_u32(code: u32) -> Self {
        match code {
            0 => Self::Ok,
            1 => Self::Eof,
            2 => Self::NoSuchFile,
            3 => Self::PermissionDenied,
            4 => Self::Failure,
            5 => Self::BadMessage,
            6 => Self::NoConnection,
            7 => Self::ConnectionLost,
            8 => Self::OpUnsupported,
            other => Self::Other(other),
        }
    }

    /// The wire value.
    pub const fn to_u32(self) -> u32 {
        match self {
            Self::Ok => 0,
            Self::Eof => 1,
            Self::NoSuchFile => 2,
            Self::PermissionDenied => 3,
            Self::Failure => 4,
            Self::BadMessage => 5,
            Self::NoConnection => 6,
            Self::ConnectionLost => 7,
            Self::OpUnsupported => 8,
            Self::Other(code) => code,
        }
    }
}

/// One entry of a `NAME` reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameEntry {
    /// File name.
    pub name: Bytes,
    /// An `ls -l` style line. Servers fill it in differently; do not parse it.
    pub long_name: Bytes,
    /// Attributes.
    pub attrs: Attributes,
}

/// The operation of a client request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// Open a file.
    Open {
        /// Path.
        path: Bytes,
        /// Open mode.
        flags: OpenFlags,
        /// Attributes for a newly created file.
        attrs: Attributes,
    },
    /// Close a file or directory handle.
    Close {
        /// The handle.
        handle: Bytes,
    },
    /// Read from a file.
    Read {
        /// The handle.
        handle: Bytes,
        /// Offset in bytes.
        offset: u64,
        /// Most bytes to return.
        len: u32,
    },
    /// Write to a file.
    Write {
        /// The handle.
        handle: Bytes,
        /// Offset in bytes.
        offset: u64,
        /// The data.
        data: Bytes,
    },
    /// Attributes of a path, not following a final symbolic link.
    Lstat {
        /// Path.
        path: Bytes,
    },
    /// Attributes of an open file.
    Fstat {
        /// The handle.
        handle: Bytes,
    },
    /// Set attributes of a path.
    SetStat {
        /// Path.
        path: Bytes,
        /// New attributes.
        attrs: Attributes,
    },
    /// Set attributes of an open file.
    FsetStat {
        /// The handle.
        handle: Bytes,
        /// New attributes.
        attrs: Attributes,
    },
    /// Open a directory for listing.
    OpenDir {
        /// Path.
        path: Bytes,
    },
    /// Read the next batch of directory entries.
    ReadDir {
        /// The handle.
        handle: Bytes,
    },
    /// Remove a file.
    Remove {
        /// Path.
        path: Bytes,
    },
    /// Create a directory.
    MkDir {
        /// Path.
        path: Bytes,
        /// Attributes for the new directory.
        attrs: Attributes,
    },
    /// Remove a directory.
    RmDir {
        /// Path.
        path: Bytes,
    },
    /// Canonicalize a path.
    RealPath {
        /// Path.
        path: Bytes,
    },
    /// Attributes of a path, following symbolic links.
    Stat {
        /// Path.
        path: Bytes,
    },
    /// Rename a file.
    Rename {
        /// Current path.
        from: Bytes,
        /// New path.
        to: Bytes,
    },
    /// Read the target of a symbolic link.
    ReadLink {
        /// Path of the link.
        path: Bytes,
    },
    /// Create a symbolic link. The fields are in the order OpenSSH uses on
    /// the wire, which is the reverse of the order in the protocol draft.
    Symlink {
        /// What the link points to.
        target: Bytes,
        /// Path of the link to create.
        link: Bytes,
    },
    /// A vendor extension request.
    Extended {
        /// Extension name.
        name: Bytes,
        /// Extension-specific data, to the end of the packet.
        data: Bytes,
    },
    /// A request type this implementation does not know. Reply with
    /// [`StatusCode::OpUnsupported`]. The body is not kept.
    Unknown {
        /// The packet type byte.
        ty: u8,
    },
}

impl Op {
    fn ty(&self) -> u8 {
        match self {
            Self::Open { .. } => OPEN,
            Self::Close { .. } => CLOSE,
            Self::Read { .. } => READ,
            Self::Write { .. } => WRITE,
            Self::Lstat { .. } => LSTAT,
            Self::Fstat { .. } => FSTAT,
            Self::SetStat { .. } => SETSTAT,
            Self::FsetStat { .. } => FSETSTAT,
            Self::OpenDir { .. } => OPENDIR,
            Self::ReadDir { .. } => READDIR,
            Self::Remove { .. } => REMOVE,
            Self::MkDir { .. } => MKDIR,
            Self::RmDir { .. } => RMDIR,
            Self::RealPath { .. } => REALPATH,
            Self::Stat { .. } => STAT,
            Self::Rename { .. } => RENAME,
            Self::ReadLink { .. } => READLINK,
            Self::Symlink { .. } => SYMLINK,
            Self::Extended { .. } => EXTENDED,
            Self::Unknown { ty } => *ty,
        }
    }
}

/// A client request: an operation tagged with a request id the server echoes
/// in its reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// Request id.
    pub id: u32,
    /// The operation.
    pub op: Op,
}

impl Request {
    fn encode(&self, out: &mut BytesMut) -> Result<(), EncodeError> {
        out.put_u8(self.op.ty());
        out.put_u32(self.id);
        match &self.op {
            Op::Open { path, flags, attrs } => {
                put_string(out, path)?;
                out.put_u32(flags.0);
                attrs.encode(out)?;
            }
            Op::Close { handle } | Op::Fstat { handle } | Op::ReadDir { handle } => {
                put_string(out, handle)?;
            }
            Op::Read {
                handle,
                offset,
                len,
            } => {
                put_string(out, handle)?;
                out.put_u64(*offset);
                out.put_u32(*len);
            }
            Op::Write {
                handle,
                offset,
                data,
            } => {
                put_string(out, handle)?;
                out.put_u64(*offset);
                put_string(out, data)?;
            }
            Op::Lstat { path }
            | Op::OpenDir { path }
            | Op::Remove { path }
            | Op::RmDir { path }
            | Op::RealPath { path }
            | Op::Stat { path }
            | Op::ReadLink { path } => put_string(out, path)?,
            Op::SetStat { path, attrs } | Op::MkDir { path, attrs } => {
                put_string(out, path)?;
                attrs.encode(out)?;
            }
            Op::FsetStat { handle, attrs } => {
                put_string(out, handle)?;
                attrs.encode(out)?;
            }
            Op::Rename { from, to } => {
                put_string(out, from)?;
                put_string(out, to)?;
            }
            Op::Symlink { target, link } => {
                put_string(out, target)?;
                put_string(out, link)?;
            }
            Op::Extended { name, data } => {
                put_string(out, name)?;
                out.extend_from_slice(data);
            }
            Op::Unknown { .. } => {}
        }
        Ok(())
    }

    fn decode(ty: u8, r: &mut Reader) -> Result<Self, DecodeError> {
        let id = r.u32()?;
        let op = match ty {
            OPEN => Op::Open {
                path: r.string()?,
                flags: OpenFlags(r.u32()?),
                attrs: Attributes::decode(r)?,
            },
            CLOSE => Op::Close {
                handle: r.string()?,
            },
            READ => Op::Read {
                handle: r.string()?,
                offset: r.u64()?,
                len: r.u32()?,
            },
            WRITE => Op::Write {
                handle: r.string()?,
                offset: r.u64()?,
                data: r.string()?,
            },
            LSTAT => Op::Lstat { path: r.string()? },
            FSTAT => Op::Fstat {
                handle: r.string()?,
            },
            SETSTAT => Op::SetStat {
                path: r.string()?,
                attrs: Attributes::decode(r)?,
            },
            FSETSTAT => Op::FsetStat {
                handle: r.string()?,
                attrs: Attributes::decode(r)?,
            },
            OPENDIR => Op::OpenDir { path: r.string()? },
            READDIR => Op::ReadDir {
                handle: r.string()?,
            },
            REMOVE => Op::Remove { path: r.string()? },
            MKDIR => Op::MkDir {
                path: r.string()?,
                attrs: Attributes::decode(r)?,
            },
            RMDIR => Op::RmDir { path: r.string()? },
            REALPATH => Op::RealPath { path: r.string()? },
            STAT => Op::Stat { path: r.string()? },
            RENAME => Op::Rename {
                from: r.string()?,
                to: r.string()?,
            },
            READLINK => Op::ReadLink { path: r.string()? },
            SYMLINK => Op::Symlink {
                target: r.string()?,
                link: r.string()?,
            },
            EXTENDED => Op::Extended {
                name: r.string()?,
                data: r.rest(),
            },
            // The body of a request we cannot interpret is skipped, so the
            // caller can still answer it.
            _ => {
                r.rest();
                Op::Unknown { ty }
            }
        };
        r.finish()?;
        Ok(Self { id, op })
    }
}

/// The body of a server reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// Result of an operation that returns no data.
    Status {
        /// The result code.
        code: StatusCode,
        /// Human-readable message.
        message: Bytes,
        /// Language tag of the message.
        language: Bytes,
    },
    /// A file or directory handle.
    Handle(Bytes),
    /// File data.
    Data(Bytes),
    /// Directory entries or a path.
    Name(Vec<NameEntry>),
    /// File attributes.
    Attrs(Attributes),
    /// The reply to an extended request.
    Extended(Bytes),
    /// A reply type this implementation does not know. The body is not kept.
    Unknown {
        /// The packet type byte.
        ty: u8,
    },
}

/// A server reply: a body tagged with the id of the request it answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    /// Id of the request this answers.
    pub id: u32,
    /// The reply.
    pub reply: Reply,
}

impl Response {
    fn encode(&self, out: &mut BytesMut) -> Result<(), EncodeError> {
        let ty = match &self.reply {
            Reply::Status { .. } => STATUS,
            Reply::Handle(_) => HANDLE,
            Reply::Data(_) => DATA,
            Reply::Name(_) => NAME,
            Reply::Attrs(_) => ATTRS,
            Reply::Extended(_) => EXTENDED_REPLY,
            Reply::Unknown { ty } => *ty,
        };
        out.put_u8(ty);
        out.put_u32(self.id);
        match &self.reply {
            Reply::Status {
                code,
                message,
                language,
            } => {
                out.put_u32(code.to_u32());
                put_string(out, message)?;
                put_string(out, language)?;
            }
            Reply::Handle(s) | Reply::Data(s) => put_string(out, s)?,
            Reply::Name(entries) => {
                out.put_u32(u32::try_from(entries.len()).map_err(|_| EncodeError)?);
                for e in entries {
                    put_string(out, &e.name)?;
                    put_string(out, &e.long_name)?;
                    e.attrs.encode(out)?;
                }
            }
            Reply::Attrs(attrs) => attrs.encode(out)?,
            Reply::Extended(data) => out.extend_from_slice(data),
            Reply::Unknown { .. } => {}
        }
        Ok(())
    }

    fn decode(ty: u8, r: &mut Reader) -> Result<Self, DecodeError> {
        let id = r.u32()?;
        let reply = match ty {
            STATUS => {
                let code = StatusCode::from_u32(r.u32()?);
                // Some servers leave out the message and language tag.
                let (message, language) = if r.remaining() == 0 {
                    (Bytes::new(), Bytes::new())
                } else {
                    (r.string()?, r.string()?)
                };
                Reply::Status {
                    code,
                    message,
                    language,
                }
            }
            HANDLE => Reply::Handle(r.string()?),
            DATA => Reply::Data(r.string()?),
            NAME => {
                let count = r.u32()? as usize;
                // An entry takes at least two length fields and attribute flags.
                if count > r.remaining() / 12 {
                    return Err(DecodeError::CountTooLarge);
                }
                let mut entries = Vec::with_capacity(count);
                for _ in 0..count {
                    entries.push(NameEntry {
                        name: r.string()?,
                        long_name: r.string()?,
                        attrs: Attributes::decode(r)?,
                    });
                }
                Reply::Name(entries)
            }
            ATTRS => Reply::Attrs(Attributes::decode(r)?),
            EXTENDED_REPLY => Reply::Extended(r.rest()),
            _ => {
                r.rest();
                Reply::Unknown { ty }
            }
        };
        r.finish()?;
        Ok(Self { id, reply })
    }
}

/// A packet sent by the client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientPacket {
    /// The opening handshake.
    Init(Hello),
    /// A request.
    Request(Request),
}

impl ClientPacket {
    /// Decodes the payload of one packet (without the length prefix).
    pub fn decode(payload: Bytes) -> Result<Self, DecodeError> {
        let mut r = Reader::new(payload);
        match r.u8()? {
            INIT => {
                let hello = Hello::decode(&mut r)?;
                Ok(Self::Init(hello))
            }
            ty => Request::decode(ty, &mut r).map(Self::Request),
        }
    }
}

impl From<Request> for ClientPacket {
    fn from(request: Request) -> Self {
        Self::Request(request)
    }
}

impl Encode for ClientPacket {
    fn encode_frame(&self, out: &mut BytesMut) -> Result<(), EncodeError> {
        frame(out, |out| match self {
            Self::Init(hello) => hello.encode(INIT, out),
            Self::Request(request) => request.encode(out),
        })
    }
}

/// A packet sent by the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerPacket {
    /// The handshake reply.
    Version(Hello),
    /// A reply to a request.
    Response(Response),
}

impl ServerPacket {
    /// Decodes the payload of one packet (without the length prefix).
    pub fn decode(payload: Bytes) -> Result<Self, DecodeError> {
        let mut r = Reader::new(payload);
        match r.u8()? {
            VERSION => {
                let hello = Hello::decode(&mut r)?;
                Ok(Self::Version(hello))
            }
            ty => Response::decode(ty, &mut r).map(Self::Response),
        }
    }
}

impl From<Response> for ServerPacket {
    fn from(response: Response) -> Self {
        Self::Response(response)
    }
}

impl Encode for ServerPacket {
    fn encode_frame(&self, out: &mut BytesMut) -> Result<(), EncodeError> {
        frame(out, |out| match self {
            Self::Version(hello) => hello.encode(VERSION, out),
            Self::Response(response) => response.encode(out),
        })
    }
}
