//! Packet framing on byte streams.

use std::time::Duration;

use bytes::Bytes;
use thirtysftp::codec::{Error, FrameReader, FrameWriter};
use thirtysftp::wire::{ClientPacket, Hello, Op, Request};
use tokio::io::{AsyncWriteExt, duplex};
use tokio::time::timeout;

fn frame(payload: &[u8]) -> Vec<u8> {
    let mut v = (payload.len() as u32).to_be_bytes().to_vec();
    v.extend_from_slice(payload);
    v
}

#[tokio::test]
async fn reads_frames_delivered_one_byte_at_a_time() {
    let (mut tx, rx) = duplex(64);
    let mut data = frame(b"hello");
    data.extend(frame(b"x"));
    let writer = tokio::spawn(async move {
        for byte in data {
            tx.write_all(&[byte]).await.unwrap();
            tokio::task::yield_now().await;
        }
    });
    let mut reader = FrameReader::new(rx, 1024);
    assert_eq!(reader.read_frame().await.unwrap().unwrap(), "hello");
    assert_eq!(reader.read_frame().await.unwrap().unwrap(), "x");
    writer.await.unwrap();
    assert!(reader.read_frame().await.unwrap().is_none());
}

#[tokio::test]
async fn several_frames_in_one_read() {
    let (mut tx, rx) = duplex(1024);
    let mut data = Vec::new();
    for i in 0..10u8 {
        data.extend(frame(&[i; 3]));
    }
    tx.write_all(&data).await.unwrap();
    drop(tx);
    let mut reader = FrameReader::new(rx, 1024);
    for i in 0..10u8 {
        assert_eq!(reader.read_frame().await.unwrap().unwrap(), vec![i; 3]);
    }
    assert!(reader.read_frame().await.unwrap().is_none());
}

#[tokio::test]
async fn end_of_stream_inside_a_frame_is_an_error() {
    for cut in [1, 3, 4, 6] {
        let (mut tx, rx) = duplex(64);
        tx.write_all(&frame(b"abcdef")[..cut]).await.unwrap();
        drop(tx);
        let mut reader = FrameReader::new(rx, 1024);
        assert!(
            matches!(reader.read_frame().await, Err(Error::UnexpectedEof)),
            "cut at {cut}"
        );
    }
}

#[tokio::test]
async fn oversized_frame_is_rejected_from_the_length_alone() {
    let (mut tx, rx) = duplex(64);
    // Only the length field is ever sent; the reader must not wait for a body.
    tx.write_all(&0x7fff_ffffu32.to_be_bytes()).await.unwrap();
    let mut reader = FrameReader::new(rx, 1024);
    let result = timeout(Duration::from_secs(5), reader.read_frame())
        .await
        .expect("must fail without waiting for the body");
    assert!(matches!(
        result,
        Err(Error::FrameTooLarge {
            len: 0x7fff_ffff,
            max: 1024
        })
    ));
}

#[tokio::test]
async fn frame_at_the_limit_is_accepted_and_one_over_is_not() {
    let (mut tx, rx) = duplex(4096);
    tx.write_all(&frame(&[7; 100])).await.unwrap();
    tx.write_all(&frame(&[7; 101])).await.unwrap();
    let mut reader = FrameReader::new(rx, 100);
    assert_eq!(reader.read_frame().await.unwrap().unwrap().len(), 100);
    assert!(matches!(
        reader.read_frame().await,
        Err(Error::FrameTooLarge { len: 101, max: 100 })
    ));
}

#[tokio::test]
async fn zero_length_frame_is_rejected() {
    let (mut tx, rx) = duplex(64);
    tx.write_all(&[0, 0, 0, 0]).await.unwrap();
    let mut reader = FrameReader::new(rx, 1024);
    assert!(matches!(reader.read_frame().await, Err(Error::EmptyFrame)));
}

#[tokio::test]
async fn cancelled_read_loses_nothing() {
    let (mut tx, rx) = duplex(64);
    let data = frame(b"partial frame");
    let mut reader = FrameReader::new(rx, 1024);
    // Deliver the frame in pieces, abandoning the read between them.
    let mut got = None;
    for piece in data.chunks(3) {
        tx.write_all(piece).await.unwrap();
        if let Ok(result) = timeout(Duration::from_millis(5), reader.read_frame()).await {
            got = Some(result.unwrap().unwrap());
        }
    }
    let got = match got {
        Some(frame) => frame,
        None => reader.read_frame().await.unwrap().unwrap(),
    };
    assert_eq!(got, "partial frame");
}

#[tokio::test]
async fn writer_survives_a_tiny_pipe() {
    // The pipe holds 8 bytes, so every flush needs many partial writes.
    let (tx, rx) = duplex(8);
    let mut writer = FrameWriter::new(tx);
    for id in 0..20 {
        writer
            .queue(&ClientPacket::Request(Request {
                id,
                op: Op::Remove {
                    path: Bytes::from(vec![b'p'; 50]),
                },
            }))
            .unwrap();
    }
    assert!(writer.queued() > 8);
    let read = tokio::spawn(async move {
        let mut reader = FrameReader::new(rx, 1024);
        let mut ids = Vec::new();
        while let Some(p) = reader.read_frame().await.unwrap() {
            let ClientPacket::Request(r) = ClientPacket::decode(p).unwrap() else {
                panic!("not a request");
            };
            ids.push(r.id);
        }
        ids
    });
    writer.flush().await.unwrap();
    assert_eq!(writer.queued(), 0);
    drop(writer);
    assert_eq!(read.await.unwrap(), (0..20).collect::<Vec<_>>());
}

#[tokio::test]
async fn cancelled_flush_resumes_where_it_stopped() {
    let (tx, rx) = duplex(8);
    let mut writer = FrameWriter::new(tx);
    for _ in 0..3 {
        writer
            .queue(&ClientPacket::Init(Hello {
                version: 3,
                extensions: vec![],
            }))
            .unwrap();
    }
    // Nobody reads yet, so this stalls part way and is dropped.
    assert!(
        timeout(Duration::from_millis(20), writer.flush())
            .await
            .is_err()
    );
    let read = tokio::spawn(async move {
        let mut reader = FrameReader::new(rx, 1024);
        let mut count = 0;
        while let Some(p) = reader.read_frame().await.unwrap() {
            assert!(matches!(
                ClientPacket::decode(p).unwrap(),
                ClientPacket::Init(_)
            ));
            count += 1;
        }
        count
    });
    writer.flush().await.unwrap();
    drop(writer);
    assert_eq!(read.await.unwrap(), 3);
}

#[tokio::test]
async fn random_streams_never_panic_and_stay_bounded() {
    let mut state = 0x1234_5678_9abc_def0u64;
    let mut next = move || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state.wrapping_mul(0x2545_f491_4f6c_dd1d)
    };
    for _ in 0..200 {
        let len = (next() % 200) as usize;
        let data: Vec<u8> = (0..len).map(|_| (next() % 4) as u8).collect();
        let (mut tx, rx) = duplex(1024);
        tx.write_all(&data).await.unwrap();
        drop(tx);
        let mut reader = FrameReader::new(rx, 16);
        while let Ok(Some(p)) = reader.read_frame().await {
            assert!(p.len() <= 16);
        }
    }
}
