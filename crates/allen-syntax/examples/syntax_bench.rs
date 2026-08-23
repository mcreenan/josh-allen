use allen_syntax::{SourceFile, SourceFileId, SyntaxLimits, TextEdit, parse, reparse};
use std::{env, hint::black_box, time::Instant};

const MAX_SOURCE_BYTES: usize = 64 * 1024 * 1024;
const DENSE_NEWLINE_COUNT: usize = 1_000_000;
const DENSE_FUNCTION_COUNT: usize = 40_000;

fn main() {
    match env::args().nth(1).as_deref() {
        None | Some("time") => timing_suite(),
        Some("memory-empty") => memory_case("empty", String::new()),
        Some("memory-max") => memory_case("maximum-bounded-comment", maximum_source()),
        Some("memory-dense-newlines") => {
            memory_case("dense-newlines", "\n".repeat(DENSE_NEWLINE_COUNT));
        }
        Some("memory-dense-tree") => memory_case("dense-function-tree", dense_function_source()),
        Some(other) => panic!("unknown benchmark mode {other:?}"),
    }
}

fn timing_suite() {
    measure_full("empty", &[""], 2_000);
    measure_full("small", &["fn f() returns Int { 1 }\n"], 2_000);
    measure_full(
        "unicode-template-heavy",
        &[include_str!(
            "../../../fuzz/seeds/parser/templates-and-unicode"
        )],
        1_000,
    );
    measure_full(
        "malformed",
        &[
            include_str!("../../../fuzz/seeds/parser/template-truncated"),
            include_str!("../../../fuzz/seeds/parser/comment-over-nesting"),
        ],
        500,
    );
    measure_full(
        "multi-module",
        &[
            include_str!("../../../examples/functions-and-effects/main.allen"),
            include_str!("../../../examples/functions-and-effects/support.allen"),
        ],
        500,
    );
    measure_incremental();
    measure_full("maximum-bounded-comment", &[&maximum_source()], 1);
}

fn measure_full(name: &str, sources: &[&str], iterations: usize) {
    let source_bytes: usize = sources.iter().map(|source| source.len()).sum();
    let started = Instant::now();
    let mut diagnostic_count = 0;
    for iteration in 0..iterations {
        for (index, text) in sources.iter().enumerate() {
            let source =
                SourceFile::new(SourceFileId::new(u32::try_from(index).unwrap()), *text).unwrap();
            let parsed = parse(black_box(&source));
            diagnostic_count += parsed.diagnostics().len();
            black_box(parsed.green());
        }
        black_box(iteration);
    }
    let elapsed = started.elapsed();
    let operations = iterations * sources.len();
    println!(
        "full fixture={name} sources={} bytes_per_iteration={source_bytes} iterations={iterations} ns_per_parse={} diagnostics={diagnostic_count}",
        sources.len(),
        elapsed.as_nanos() / operations as u128,
    );
}

fn measure_incremental() {
    let mut text = String::from("fn main() returns Int {\n");
    for index in 0..256 {
        text.push_str(&format!("  let value{index} = {index};\n"));
    }
    text.push_str("  value255\n}\n");
    let source = SourceFile::from_string(SourceFileId::new(0), text).unwrap();
    let parsed = parse(&source);
    let edit = TextEdit::new(&source, 2, 3, "\t").unwrap();
    let sample = reparse(&parsed, &source, &edit).unwrap();
    assert!(!sample.statistics().full_fallback());
    let fresh_source = sample.source().clone();
    let iterations = 5_000;

    let started = Instant::now();
    for _ in 0..iterations {
        let result = reparse(black_box(&parsed), black_box(&source), black_box(&edit)).unwrap();
        black_box(result.parse().green());
    }
    let incremental = started.elapsed();

    let started = Instant::now();
    for _ in 0..iterations {
        let result = parse(black_box(&fresh_source));
        black_box(result.green());
    }
    let full = started.elapsed();
    let stats = sample.statistics();
    println!(
        "incremental fixture=wide-function source_bytes={} iterations={iterations} incremental_ns_per_edit={} full_ns_per_parse={} source_bytes_copied={} bytes_relexed={} old_nodes_replaced={} new_nodes_replaced={} snapshot_checks={} cached_error_checks={} positional_token_lookups={} lookup_path_nodes={} full_fallback={}",
        source.text().len(),
        incremental.as_nanos() / iterations,
        full.as_nanos() / iterations,
        stats.source_bytes_copied(),
        stats.bytes_relexed(),
        stats.old_nodes_replaced(),
        stats.new_nodes_replaced(),
        stats.source_snapshot_checks(),
        stats.cached_error_checks(),
        stats.positional_token_lookups(),
        stats.token_lookup_path_nodes(),
        stats.full_fallback(),
    );
}

fn memory_case(name: &str, text: String) {
    let limits = SyntaxLimits::DEFAULT;
    let source = SourceFile::from_string(SourceFileId::new(0), text).unwrap();
    let parsed = parse(&source);
    let root = parsed.syntax();
    let nodes = root.descendants().count();
    let tokens = root
        .descendants_with_tokens()
        .filter_map(rowan::NodeOrToken::into_token)
        .count();
    println!(
        "memory fixture={name} source_bytes={} nodes={nodes} tokens={tokens} diagnostics={} token_cap={} node_cap={}",
        source.text().len(),
        parsed.diagnostics().len(),
        limits.tokens,
        limits.nodes,
    );
    black_box((source, parsed));
}

fn dense_function_source() -> String {
    let mut text = String::with_capacity(DENSE_FUNCTION_COUNT * 32);
    for index in 0..DENSE_FUNCTION_COUNT {
        text.push_str("fn f");
        text.push_str(&index.to_string());
        text.push_str("() returns Int { ");
        text.push_str(&index.to_string());
        text.push_str(" }\n");
    }
    text
}

fn maximum_source() -> String {
    let mut text = String::with_capacity(MAX_SOURCE_BYTES);
    text.push_str("/*");
    text.extend(std::iter::repeat_n('x', MAX_SOURCE_BYTES - 4));
    text.push_str("*/");
    assert_eq!(text.len(), MAX_SOURCE_BYTES);
    text
}
