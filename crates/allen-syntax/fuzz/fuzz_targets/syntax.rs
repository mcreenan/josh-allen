#![no_main]

use allen_syntax::{SourceFile, SourceFileId, SyntaxKind, lex, parse};
use libfuzzer_sys::fuzz_target;

const MAX_INPUT_BYTES: usize = 256 * 1024;

fuzz_target!(|data: &[u8]| {
    let data = &data[..data.len().min(MAX_INPUT_BYTES)];
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let source = SourceFile::new(SourceFileId::new(0), text).expect("bounded UTF-8 source");

    let lexed = lex(&source);
    assert_eq!(lexed.round_trip(&source), text, "lexer must be lossless");
    assert_eq!(
        lexed.tokens().last().map(|token| token.kind()),
        Some(SyntaxKind::Eof),
        "EOF must terminate the lexer stream",
    );

    let parsed = parse(&source);
    parsed.assert_round_trip(&source);
    let root = parsed.syntax();
    assert_eq!(root.kind(), SyntaxKind::Source, "tree root must be Source");
    assert_eq!(root.to_string(), text, "syntax tree must be lossless");
    assert_eq!(
        root.last_token().map(|token| token.kind()),
        Some(SyntaxKind::Eof),
        "EOF must terminate the tree token stream",
    );

    let printed = root.to_string();
    let printed_source =
        SourceFile::new(SourceFileId::new(0), printed).expect("printed source range");
    let reparsed = parse(&printed_source);
    assert_eq!(parsed.green(), reparsed.green(), "parse/print/parse tree");
    assert_eq!(
        parsed.diagnostics(),
        reparsed.diagnostics(),
        "parse/print/parse diagnostics",
    );
});
