//! Any bytes as a stream of frames. The reader must not panic, must never
//! return a frame over its limit, and must end.
#![no_main]

use libfuzzer_sys::fuzz_target;
use thirtysftp::codec::FrameReader;

const LIMIT: u32 = 4096;

fuzz_target!(|data: &[u8]| {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let mut reader = FrameReader::new(data, LIMIT);
        let mut total = 0usize;
        while let Ok(Some(frame)) = reader.read_frame().await {
            assert!(!frame.is_empty() && frame.len() <= LIMIT as usize);
            total += frame.len() + 4;
        }
        assert!(total <= data.len());
    });
});
