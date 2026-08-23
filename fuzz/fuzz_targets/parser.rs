#![no_main]

use libfuzzer_sys::fuzz_target;

const MAX_INPUT_BYTES: usize = 256 * 1024;

fn exercise_loaded_source(input: &[u8]) {
    match std::str::from_utf8(input) {
        Ok(source) => {
            let _ = allen_compiler::compile(source);
        }
        Err(error) => {
            assert!(error.valid_up_to() < input.len());
            assert!(input.get(error.valid_up_to()).is_some());
            let _ = error.error_len();
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let input = &data[..data.len().min(MAX_INPUT_BYTES)];
    exercise_loaded_source(input);
});
