//! Any bytes as the payload of one packet, in both directions. Decoding must
//! not panic, and whatever decodes must encode again to something that
//! decodes to the same value.
#![no_main]

use bytes::{Bytes, BytesMut};
use libfuzzer_sys::fuzz_target;
use thirtysftp::wire::{ClientPacket, Encode, ServerPacket};

fn payload_of(packet: &impl Encode) -> Bytes {
    let mut frame = BytesMut::new();
    packet.encode_frame(&mut frame).expect("a decoded packet fits");
    frame.freeze().slice(4..)
}

fuzz_target!(|data: &[u8]| {
    let payload = Bytes::copy_from_slice(data);
    if let Ok(packet) = ClientPacket::decode(payload.clone()) {
        let again = ClientPacket::decode(payload_of(&packet)).expect("re-encoded packet decodes");
        assert_eq!(again, packet);
    }
    if let Ok(packet) = ServerPacket::decode(payload) {
        let again = ServerPacket::decode(payload_of(&packet)).expect("re-encoded packet decodes");
        assert_eq!(again, packet);
    }
});
