#![no_main]

use allen_schema::{SchemaLimits, ToolSchema};
use libfuzzer_sys::fuzz_target;

const MAX_INPUT_BYTES: usize = 256 * 1024;

fuzz_target!(|data: &[u8]| {
    let input = &data[..data.len().min(MAX_INPUT_BYTES)];
    let schema = String::from_utf8_lossy(input);
    let _ = ToolSchema::parse(&schema, &SchemaLimits::default());
});
