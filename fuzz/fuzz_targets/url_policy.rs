#![no_main]

use allen_http_get::CanonicalOrigin;
use libfuzzer_sys::fuzz_target;

const MAX_INPUT_BYTES: usize = 16 * 1024;

fuzz_target!(|data: &[u8]| {
    let input = &data[..data.len().min(MAX_INPUT_BYTES)];
    let url = String::from_utf8_lossy(input);
    let _ = CanonicalOrigin::parse(&url);
});
