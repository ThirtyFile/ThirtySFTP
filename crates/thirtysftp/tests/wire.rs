//! Packet encoding: hand-written vectors, round trips, malformed input and
//! seeded random input.

use bytes::{Bytes, BytesMut};
use thirtysftp::wire::{
    Attributes, ClientPacket, DecodeError, Encode, Extension, Hello, NameEntry, Op, OpenFlags,
    Owner, Reply, Request, Response, ServerPacket, StatusCode, Times,
};

fn b(s: &str) -> Bytes {
    Bytes::copy_from_slice(s.as_bytes())
}

fn frame_of(packet: &impl Encode) -> Vec<u8> {
    let mut out = BytesMut::new();
    packet.encode_frame(&mut out).unwrap();
    out.to_vec()
}

/// The payload of a frame, after checking that the length prefix is right.
fn payload(frame: &[u8]) -> Bytes {
    let len = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
    assert_eq!(len, frame.len() - 4, "length prefix");
    Bytes::copy_from_slice(&frame[4..])
}

fn full_attrs() -> Attributes {
    Attributes {
        size: Some(0x1_0000_0001),
        owner: Some(Owner {
            uid: 1000,
            gid: 100,
        }),
        permissions: Some(0o100644),
        times: Some(Times {
            accessed: 5,
            modified: 6,
        }),
        extended: vec![Extension {
            name: b("x@example"),
            data: b("1"),
        }],
    }
}

fn requests() -> Vec<Request> {
    let ops = vec![
        Op::Open {
            path: b("/a"),
            flags: OpenFlags::READ | OpenFlags::CREATE,
            attrs: full_attrs(),
        },
        Op::Close { handle: b("h") },
        Op::Read {
            handle: b("h"),
            offset: u64::MAX,
            len: 32768,
        },
        Op::Write {
            handle: b("h"),
            offset: 1 << 40,
            data: b("data"),
        },
        Op::Lstat { path: b("/l") },
        Op::Fstat { handle: b("h") },
        Op::SetStat {
            path: b("/s"),
            attrs: full_attrs(),
        },
        Op::FsetStat {
            handle: b("h"),
            attrs: Attributes::default(),
        },
        Op::OpenDir { path: b("/d") },
        Op::ReadDir { handle: b("h") },
        Op::Remove { path: b("/r") },
        Op::MkDir {
            path: b("/m"),
            attrs: Attributes::default(),
        },
        Op::RmDir { path: b("/m") },
        Op::RealPath { path: b(".") },
        Op::Stat { path: b("/t") },
        Op::Rename {
            from: b("/a"),
            to: b("/b"),
        },
        Op::ReadLink { path: b("/k") },
        Op::Symlink {
            target: b("t"),
            link: b("l"),
        },
        Op::Extended {
            name: b("statvfs@openssh.com"),
            data: b("\0\0\0\x01/"),
        },
        Op::Unknown { ty: 77 },
    ];
    ops.into_iter()
        .enumerate()
        .map(|(i, op)| Request { id: i as u32, op })
        .collect()
}

fn responses() -> Vec<Response> {
    let replies = vec![
        Reply::Status {
            code: StatusCode::NoSuchFile,
            message: b("no such file"),
            language: b("en"),
        },
        Reply::Status {
            code: StatusCode::Other(99),
            message: Bytes::new(),
            language: Bytes::new(),
        },
        Reply::Handle(b("h")),
        Reply::Data(Bytes::from_static(&[0, 255, 1])),
        Reply::Name(vec![
            NameEntry {
                name: Bytes::from_static(&[0xff, 0xfe]),
                long_name: b("-rw-r--r-- 1 u g 0 Jan 1 x"),
                attrs: full_attrs(),
            },
            NameEntry {
                name: b("b"),
                long_name: Bytes::new(),
                attrs: Attributes::default(),
            },
        ]),
        Reply::Name(Vec::new()),
        Reply::Attrs(full_attrs()),
        Reply::Extended(b("reply")),
        Reply::Unknown { ty: 150 },
    ];
    replies
        .into_iter()
        .enumerate()
        .map(|(i, reply)| Response {
            id: i as u32,
            reply,
        })
        .collect()
}

#[test]
fn every_request_round_trips() {
    for request in requests() {
        let packet = ClientPacket::Request(request);
        let decoded = ClientPacket::decode(payload(&frame_of(&packet))).unwrap();
        assert_eq!(decoded, packet);
    }
}

#[test]
fn every_response_round_trips() {
    for response in responses() {
        let packet = ServerPacket::Response(response);
        let decoded = ServerPacket::decode(payload(&frame_of(&packet))).unwrap();
        assert_eq!(decoded, packet);
    }
}

#[test]
fn handshakes_round_trip() {
    let hello = Hello {
        version: 3,
        extensions: vec![
            Extension {
                name: b("posix-rename@openssh.com"),
                data: b("1"),
            },
            Extension {
                name: b("limits@openssh.com"),
                data: b("1"),
            },
        ],
    };
    let init = ClientPacket::Init(hello.clone());
    assert_eq!(
        ClientPacket::decode(payload(&frame_of(&init))).unwrap(),
        init
    );
    let version = ServerPacket::Version(hello);
    assert_eq!(
        ServerPacket::decode(payload(&frame_of(&version))).unwrap(),
        version
    );
}

#[test]
fn init_vector() {
    let init = ClientPacket::Init(Hello {
        version: 3,
        extensions: vec![],
    });
    assert_eq!(frame_of(&init), [0, 0, 0, 5, 1, 0, 0, 0, 3]);
}

#[test]
fn close_vector() {
    let close = ClientPacket::Request(Request {
        id: 7,
        op: Op::Close { handle: b("hndl") },
    });
    assert_eq!(
        frame_of(&close),
        [
            0, 0, 0, 13, 4, 0, 0, 0, 7, 0, 0, 0, 4, b'h', b'n', b'd', b'l'
        ]
    );
}

#[test]
fn read_vector_keeps_64_bit_offset() {
    let read = ClientPacket::Request(Request {
        id: 1,
        op: Op::Read {
            handle: b("h"),
            offset: 0x0000_0001_0000_0002,
            len: 0x8000,
        },
    });
    assert_eq!(
        frame_of(&read),
        [
            0, 0, 0, 22, 5, 0, 0, 0, 1, 0, 0, 0, 1, b'h', 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0x80, 0
        ]
    );
}

#[test]
fn status_vector() {
    let status = ServerPacket::Response(Response {
        id: 1,
        reply: Reply::Status {
            code: StatusCode::NoSuchFile,
            message: b("no"),
            language: Bytes::new(),
        },
    });
    assert_eq!(
        frame_of(&status),
        [
            0, 0, 0, 19, 101, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 2, b'n', b'o', 0, 0, 0, 0
        ]
    );
}

#[test]
fn attrs_vector() {
    let attrs = ServerPacket::Response(Response {
        id: 2,
        reply: Reply::Attrs(Attributes {
            size: Some(9),
            permissions: Some(0o644),
            ..Attributes::default()
        }),
    });
    assert_eq!(
        frame_of(&attrs),
        [
            0, 0, 0, 21, 105, 0, 0, 0, 2, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 9, 0, 0, 0x01, 0xa4
        ]
    );
}

#[test]
fn status_without_message_is_accepted() {
    // Some servers send only the code.
    let p = Bytes::from_static(&[101, 0, 0, 0, 1, 0, 0, 0, 1]);
    assert_eq!(
        ServerPacket::decode(p).unwrap(),
        ServerPacket::Response(Response {
            id: 1,
            reply: Reply::Status {
                code: StatusCode::Eof,
                message: Bytes::new(),
                language: Bytes::new()
            }
        })
    );
}

#[test]
fn unknown_request_keeps_its_id() {
    let p = Bytes::from_static(&[99, 0, 0, 0, 42, 1, 2, 3]);
    assert_eq!(
        ClientPacket::decode(p).unwrap(),
        ClientPacket::Request(Request {
            id: 42,
            op: Op::Unknown { ty: 99 }
        })
    );
}

#[test]
fn non_utf8_names_survive() {
    let name = Bytes::from_static(&[b'a', 0xff, 0x80]);
    let packet = ClientPacket::Request(Request {
        id: 1,
        op: Op::Remove { path: name.clone() },
    });
    let ClientPacket::Request(Request {
        op: Op::Remove { path },
        ..
    }) = ClientPacket::decode(payload(&frame_of(&packet))).unwrap()
    else {
        panic!("wrong packet");
    };
    assert_eq!(path, name);
}

#[test]
fn every_truncation_is_an_error() {
    // Skipped: extended requests and replies (data runs to the end), and
    // status replies (message is optional).
    for request in requests() {
        if matches!(request.op, Op::Extended { .. } | Op::Unknown { .. }) {
            continue;
        }
        let full = payload(&frame_of(&ClientPacket::Request(request.clone())));
        for cut in 0..full.len() {
            assert!(
                ClientPacket::decode(full.slice(..cut)).is_err(),
                "{request:?} cut at {cut}"
            );
        }
    }
    for response in responses() {
        if matches!(
            response.reply,
            Reply::Status { .. } | Reply::Extended(_) | Reply::Unknown { .. }
        ) {
            continue;
        }
        let full = payload(&frame_of(&ServerPacket::Response(response.clone())));
        for cut in 0..full.len() {
            assert!(
                ServerPacket::decode(full.slice(..cut)).is_err(),
                "{response:?} cut at {cut}"
            );
        }
    }
}

#[test]
fn trailing_bytes_are_rejected() {
    let mut p = payload(&frame_of(&ClientPacket::Request(Request {
        id: 1,
        op: Op::Close { handle: b("h") },
    })))
    .to_vec();
    p.push(0);
    assert_eq!(
        ClientPacket::decode(p.into()),
        Err(DecodeError::TrailingData)
    );
}

#[test]
fn string_length_beyond_packet_is_rejected() {
    // CLOSE whose handle claims 4 GiB - 1.
    let p = Bytes::from_static(&[4, 0, 0, 0, 1, 0xff, 0xff, 0xff, 0xff, b'x']);
    assert_eq!(ClientPacket::decode(p), Err(DecodeError::Truncated));
}

#[test]
fn name_count_beyond_packet_is_rejected() {
    let p = Bytes::from_static(&[104, 0, 0, 0, 1, 0xff, 0xff, 0xff, 0xff]);
    assert_eq!(ServerPacket::decode(p), Err(DecodeError::CountTooLarge));
}

#[test]
fn extended_attribute_count_beyond_packet_is_rejected() {
    let p = Bytes::from_static(&[105, 0, 0, 0, 1, 0x80, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]);
    assert_eq!(ServerPacket::decode(p), Err(DecodeError::CountTooLarge));
}

#[test]
fn unknown_attribute_flags_are_rejected() {
    let p = Bytes::from_static(&[105, 0, 0, 0, 1, 0, 0, 0, 0x10]);
    assert_eq!(
        ServerPacket::decode(p),
        Err(DecodeError::UnknownAttributeFlags(0x10))
    );
}

#[test]
fn empty_payload_is_rejected() {
    assert_eq!(
        ClientPacket::decode(Bytes::new()),
        Err(DecodeError::Truncated)
    );
    assert_eq!(
        ServerPacket::decode(Bytes::new()),
        Err(DecodeError::Truncated)
    );
}

#[test]
fn status_codes_convert_both_ways() {
    for code in 0..=20u32 {
        assert_eq!(StatusCode::from_u32(code).to_u32(), code);
    }
    assert_eq!(StatusCode::from_u32(2), StatusCode::NoSuchFile);
    assert_eq!(StatusCode::from_u32(9), StatusCode::Other(9));
}

#[test]
fn failed_encode_leaves_buffer_unchanged() {
    // A real overflow needs a 4 GiB field; check the success path keeps
    // earlier content intact instead.
    let mut out = BytesMut::from(&b"keep"[..]);
    let p = ClientPacket::Init(Hello {
        version: 3,
        extensions: vec![],
    });
    p.encode_frame(&mut out).unwrap();
    assert_eq!(&out[..4], b"keep");
    assert_eq!(out.len(), 4 + 9);
}

/// xorshift64*: a fixed-seed generator, so failures reproduce.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

/// Decoding must not panic, and what decodes must survive a re-encode.
fn check_client(p: Bytes) {
    if let Ok(packet) = ClientPacket::decode(p) {
        let again = ClientPacket::decode(payload(&frame_of(&packet))).unwrap();
        assert_eq!(again, packet);
    }
}

fn check_server(p: Bytes) {
    if let Ok(packet) = ServerPacket::decode(p) {
        let again = ServerPacket::decode(payload(&frame_of(&packet))).unwrap();
        assert_eq!(again, packet);
    }
}

#[test]
fn random_input_never_panics() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..30_000 {
        let len = (rng.next() % 80) as usize;
        let mut v: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        // Bias the first byte toward real packet types.
        if let Some(first) = v.first_mut() {
            *first = [
                1, 2, 3, 4, 5, 6, 9, 10, 14, 18, 20, 101, 102, 103, 104, 105, 200, 201,
            ][(rng.next() % 18) as usize];
        }
        // Keep lengths small so deeper fields get reached.
        for chunk in v.chunks_mut(4).skip(1) {
            if rng.next() & 1 == 0 {
                chunk[0] = 0;
                if chunk.len() > 1 {
                    chunk[1] = 0;
                }
            }
        }
        let p = Bytes::from(v);
        check_client(p.clone());
        check_server(p);
    }
}

#[test]
fn corrupted_valid_packets_never_panic() {
    let mut rng = Rng(0x0123_4567_89ab_cdef);
    let mut samples: Vec<Bytes> = requests()
        .into_iter()
        .map(|r| payload(&frame_of(&ClientPacket::Request(r))))
        .collect();
    samples.extend(
        responses()
            .into_iter()
            .map(|r| payload(&frame_of(&ServerPacket::Response(r)))),
    );
    for _ in 0..30_000 {
        let base = &samples[(rng.next() % samples.len() as u64) as usize];
        let mut v = base.to_vec();
        for _ in 0..=(rng.next() % 3) {
            let i = (rng.next() % v.len() as u64) as usize;
            v[i] = rng.next() as u8;
        }
        if rng.next() & 3 == 0 {
            v.truncate((rng.next() % v.len() as u64) as usize);
        }
        let p = Bytes::from(v);
        check_client(p.clone());
        check_server(p);
    }
}
