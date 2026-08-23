#![no_main]

use allen_bytecode::{DecodeLimits, decode};
use libfuzzer_sys::fuzz_target;

const MAX_INPUT_BYTES: usize = 1024 * 1024;

fuzz_target!(|data: &[u8]| {
    let input = &data[..data.len().min(MAX_INPUT_BYTES)];
    let limits = DecodeLimits {
        artifact_bytes: MAX_INPUT_BYTES,
        section_bytes: MAX_INPUT_BYTES,
        string_bytes: 64 * 1024,
        table_entries: 4_096,
        functions: 1_024,
        registers_per_function: 4_096,
        instructions_per_function: 16_384,
        operands_per_instruction: 4_096,
        type_depth: 32,
        debug_records: 4_096,
        verifier_state_bytes: MAX_INPUT_BYTES,
        expanded_type_nodes: 4_096,
        decoded_model_bytes: MAX_INPUT_BYTES,
    };
    let _ = decode(input, &limits);
});
