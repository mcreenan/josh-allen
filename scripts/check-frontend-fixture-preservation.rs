//! Compare the extracted frontend test body and raw literals with its verified baseline.

use std::{env, fs, process::Command};

const BASELINE_COMMIT: &str = "6ba0b7e7f9b4ac39a54e87d213f743f6170ccefa";
const TESTS_PATH: &str = "crates/allen-compiler/src/frontend/tests.rs";
const EXPECTED_RAW_LITERAL_COUNT: usize = 246;

#[derive(Debug)]
struct RawLiteral<'a> {
    payload: &'a [u8],
    line: usize,
}

fn raw_literal_start(source: &[u8], offset: usize) -> Option<(usize, usize)> {
    if offset > 0 && (source[offset - 1].is_ascii_alphanumeric() || source[offset - 1] == b'_') {
        return None;
    }
    let mut cursor = match source.get(offset..) {
        Some([b'r', ..]) => offset + 1,
        Some([b'b', b'r', ..]) => offset + 2,
        _ => return None,
    };
    let mut hashes = 0;
    while source.get(cursor) == Some(&b'#') {
        cursor += 1;
        hashes += 1;
    }
    (source.get(cursor) == Some(&b'"')).then_some((cursor + 1, hashes))
}

fn raw_literal_end(source: &[u8], content: usize, hashes: usize) -> Option<(usize, usize)> {
    let mut cursor = content;
    while cursor < source.len() {
        if source[cursor] == b'"'
            && source
                .get(cursor + 1..cursor + 1 + hashes)
                .is_some_and(|suffix| suffix.iter().all(|byte| *byte == b'#'))
        {
            return Some((cursor, cursor + 1 + hashes));
        }
        cursor += 1;
    }
    None
}

fn skip_quoted(source: &[u8], mut cursor: usize, delimiter: u8) -> usize {
    cursor += 1;
    while cursor < source.len() {
        match source[cursor] {
            b'\\' => cursor = (cursor + 2).min(source.len()),
            byte if byte == delimiter => return cursor + 1,
            _ => cursor += 1,
        }
    }
    cursor
}

fn extract_raw_literals(source: &str) -> Result<Vec<RawLiteral<'_>>, String> {
    let bytes = source.as_bytes();
    let mut literals = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes.get(cursor..cursor + 2) == Some(b"//") {
            cursor = bytes[cursor..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(bytes.len(), |relative| cursor + relative + 1);
            continue;
        }
        if bytes.get(cursor..cursor + 2) == Some(b"/*") {
            let mut depth = 1_usize;
            cursor += 2;
            while cursor < bytes.len() && depth > 0 {
                if bytes.get(cursor..cursor + 2) == Some(b"/*") {
                    depth += 1;
                    cursor += 2;
                } else if bytes.get(cursor..cursor + 2) == Some(b"*/") {
                    depth -= 1;
                    cursor += 2;
                } else {
                    cursor += 1;
                }
            }
            if depth != 0 {
                return Err("unterminated Rust block comment while scanning fixtures".to_owned());
            }
            continue;
        }
        if let Some((content, hashes)) = raw_literal_start(bytes, cursor) {
            let Some((payload_end, literal_end)) = raw_literal_end(bytes, content, hashes) else {
                return Err(format!("unterminated raw literal at byte {cursor}"));
            };
            literals.push(RawLiteral {
                payload: &bytes[content..payload_end],
                line: bytes[..content]
                    .iter()
                    .filter(|byte| **byte == b'\n')
                    .count()
                    + 1,
            });
            cursor = literal_end;
            continue;
        }
        if bytes.get(cursor..cursor + 2) == Some(b"b\"") {
            cursor = skip_quoted(bytes, cursor + 1, b'"');
            continue;
        }
        if bytes[cursor] == b'"' {
            cursor = skip_quoted(bytes, cursor, b'"');
            continue;
        }
        if bytes[cursor] == b'\'' {
            cursor = skip_quoted(bytes, cursor, b'\'');
            continue;
        }
        cursor += 1;
    }
    Ok(literals)
}

fn main() -> Result<(), String> {
    let repo = env::args()
        .nth(1)
        .ok_or_else(|| "usage: check-frontend-fixture-preservation <repo>".to_owned())?;
    let baseline = Command::new("git")
        .args(["show", &format!("{BASELINE_COMMIT}:{TESTS_PATH}")])
        .current_dir(&repo)
        .output()
        .map_err(|error| format!("cannot run git show for fixture baseline: {error}"))?;
    if !baseline.status.success() {
        return Err(format!(
            "cannot read fixture baseline {BASELINE_COMMIT}: {}",
            String::from_utf8_lossy(&baseline.stderr)
        ));
    }
    let baseline = String::from_utf8(baseline.stdout)
        .map_err(|_| "baseline frontend tests are not valid UTF-8".to_owned())?;
    let current = fs::read_to_string(format!("{repo}/{TESTS_PATH}"))
        .map_err(|error| format!("cannot read extracted frontend tests: {error}"))?;
    if current != baseline {
        return Err(format!(
            "extracted frontend tests differ from current-language baseline {BASELINE_COMMIT}"
        ));
    }

    let baseline_literals = extract_raw_literals(&baseline)?;
    let current_literals = extract_raw_literals(&current)?;

    if baseline_literals.len() != EXPECTED_RAW_LITERAL_COUNT {
        return Err(format!(
            "baseline raw literal inventory changed: expected {EXPECTED_RAW_LITERAL_COUNT}, found {}",
            baseline_literals.len()
        ));
    }
    if current_literals.len() != baseline_literals.len() {
        return Err(format!(
            "raw literal count changed: baseline {}, current {}",
            baseline_literals.len(),
            current_literals.len()
        ));
    }

    let differences = baseline_literals
        .iter()
        .zip(&current_literals)
        .enumerate()
        .filter(|(_, (baseline, current))| baseline.payload != current.payload)
        .collect::<Vec<_>>();
    if !differences.is_empty() {
        for (index, (baseline, current)) in &differences {
            eprintln!(
                "raw literal {} differs: baseline line {}, current line {}, baseline {} bytes, current {} bytes",
                index + 1,
                baseline.line,
                current.line,
                baseline.payload.len(),
                current.payload.len()
            );
        }
        return Err(format!(
            "{} of {} raw frontend fixture payloads differ from {BASELINE_COMMIT}",
            differences.len(),
            baseline_literals.len()
        ));
    }

    println!(
        "frontend raw fixture payloads: 0 differences across {} literals against {BASELINE_COMMIT}",
        baseline_literals.len()
    );
    Ok(())
}
