use allen_syntax::{
    Parse, ReparseEntryPoint, ReparseFallback, SourceFile, SourceFileId, SyntaxKind, SyntaxLimits,
    TextEdit, TextEditError, TextRangeError, parse, parse_with_limits, reparse,
};
use std::sync::Arc;

fn assert_full_parse_equivalent(actual: &Parse, source: &SourceFile, limits: SyntaxLimits) {
    let expected = parse_with_limits(source, limits);
    assert_eq!(actual.green(), expected.green(), "concrete tree differs");
    assert_eq!(
        actual.diagnostics(),
        expected.diagnostics(),
        "ordered diagnostics differ"
    );
    assert_eq!(actual.has_errors(), expected.has_errors());
    assert_eq!(actual.source_id(), expected.source_id());
    actual.assert_round_trip(source);
}

fn byte_range(text: &str, needle: &str) -> (usize, usize) {
    let start = text.find(needle).expect("fixture substring");
    (start, start + needle.len())
}

#[test]
fn text_edits_validate_identity_utf8_ranges_and_snapshots() {
    let shared: Arc<str> = Arc::from("shared source");
    let shared_source = SourceFile::new(SourceFileId::new(6), shared).unwrap();
    assert_eq!(shared_source.text(), "shared source");
    let owned = String::from("owned source");
    let owned_bytes = owned.as_ptr();
    let owned_source = SourceFile::from_string(SourceFileId::new(6), owned).unwrap();
    assert_eq!(owned_source.text().as_ptr(), owned_bytes);

    let source = SourceFile::new(SourceFileId::new(7), "a🦀z").unwrap();
    assert_eq!(
        TextEdit::new(&source, 5, 1, "x"),
        Err(TextRangeError::Reversed { start: 5, end: 1 })
    );
    assert_eq!(
        TextEdit::new(&source, 2, 5, "x"),
        Err(TextRangeError::NotCharBoundary { offset: 2 })
    );
    assert_eq!(
        TextEdit::new(&source, 5, 7, "x"),
        Err(TextRangeError::OutOfBounds {
            end: 7,
            source_len: 6,
        })
    );

    let edit = TextEdit::new(&source, 1, 5, "海").unwrap();
    let edited = edit.apply(&source).unwrap();
    assert_eq!(edited.id(), source.id());
    assert_eq!(edited.text(), "a海z");

    let other = SourceFile::new(SourceFileId::new(8), source.text()).unwrap();
    assert_eq!(
        edit.apply(&other),
        Err(TextEditError::SourceMismatch {
            edit_source: source.id(),
            actual_source: other.id(),
        })
    );
    assert!(matches!(
        reparse(&parse(&source), &other, &edit),
        Err(TextEditError::ParseSourceMismatch {
            parse_source,
            actual_source,
        }) if parse_source == source.id() && actual_source == other.id()
    ));

    let stale = SourceFile::new(SourceFileId::new(7), "b🦀z").unwrap();
    assert_eq!(
        edit.apply(&stale),
        Err(TextEditError::StaleSourceSnapshot {
            source: source.id(),
        })
    );
    let reconstructed = SourceFile::new(SourceFileId::new(7), "a🦀z").unwrap();
    assert_eq!(
        edit.apply(&reconstructed),
        Err(TextEditError::StaleSourceSnapshot {
            source: source.id(),
        })
    );
}

#[test]
fn whitespace_edits_are_local_and_newline_edits_fallback() {
    let source = SourceFile::new(
        SourceFileId::new(1),
        "fn  alpha() returns Int {\n    let value = 1;\n    value\n}\n",
    )
    .unwrap();
    let parsed = parse(&source);

    let spaces = TextEdit::new(&source, 2, 4, "\t").unwrap();
    let first = reparse(&parsed, &source, &spaces).unwrap();
    let stats = first.statistics();
    assert!(!stats.full_fallback());
    assert_eq!(
        stats.entry_point(),
        ReparseEntryPoint::Token(SyntaxKind::Whitespace)
    );
    assert_eq!(stats.bytes_relexed(), 1);
    assert_eq!(stats.source_bytes_copied(), first.source().text().len());
    assert!(stats.bytes_relexed() < first.source().text().len());
    assert!(stats.old_nodes_replaced() < parsed.syntax().descendants().count());
    assert_eq!(stats.old_nodes_replaced(), stats.new_nodes_replaced());
    assert_eq!(stats.source_snapshot_checks(), 2);
    assert_eq!(stats.cached_error_checks(), 1);
    assert_eq!(stats.positional_token_lookups(), 2);
    assert_eq!(
        stats.token_lookup_path_nodes(),
        stats.old_nodes_replaced() * 2
    );
    assert!(stats.token_lookup_path_nodes() < parsed.syntax().descendants().count());
    assert_full_parse_equivalent(first.parse(), first.source(), parsed.limits());

    let (newline_start, newline_end) = byte_range(first.source().text(), "\n");
    let newline = TextEdit::new(first.source(), newline_start, newline_end, "\r\n").unwrap();
    let second = reparse(first.parse(), first.source(), &newline).unwrap();
    assert!(second.statistics().full_fallback());
    assert_eq!(second.statistics().entry_point(), ReparseEntryPoint::Source);
    assert_eq!(
        second.statistics().fallback(),
        Some(ReparseFallback::UnsupportedTokenKind)
    );
    assert_full_parse_equivalent(second.parse(), second.source(), parsed.limits());

    let repeated = reparse(first.parse(), first.source(), &newline).unwrap();
    assert_eq!(second.statistics(), repeated.statistics());
    assert_eq!(second.parse().green(), repeated.parse().green());
}

#[test]
fn crlf_adjacency_always_falls_back_and_matches_full_parsing() {
    let fixtures = [
        ("fn f() returns Int { 1 }\r\n\n", "\r\n", 1, 2, ""),
        ("fn f() returns Int { 1 }\r\n\n", "\r\n", 0, 1, ""),
        ("fn f() returns Int { 1 }\n\n", "\n\n", 0, 1, "\r"),
        ("fn f() returns Int { 1 }\r\n", "\r\n", 1, 1, "\r"),
    ];
    for (text, needle, relative_start, relative_end, replacement) in fixtures {
        let source = SourceFile::new(SourceFileId::new(11), text).unwrap();
        let parsed = parse(&source);
        let base = text.find(needle).unwrap();
        let edit = TextEdit::new(
            &source,
            base + relative_start,
            base + relative_end,
            replacement,
        )
        .unwrap();
        let result = reparse(&parsed, &source, &edit).unwrap();
        assert!(result.statistics().full_fallback(), "fixture {text:?}");
        assert_full_parse_equivalent(result.parse(), result.source(), parsed.limits());
    }
}

#[test]
fn quoted_literal_edits_are_safe_and_unicode_aware() {
    let source = SourceFile::new(
        SourceFileId::new(9),
        "fn literals() returns String {\n  let bytes = b\"ok\";\n  \"crab\"\n}\n",
    )
    .unwrap();
    let parsed = parse(&source);
    assert!(!parsed.has_errors());

    let (start, end) = byte_range(source.text(), "crab");
    let edit = TextEdit::new(&source, start, end, "🦀e\u{301}").unwrap();
    let string_result = reparse(&parsed, &source, &edit).unwrap();
    assert_eq!(
        string_result.statistics().entry_point(),
        ReparseEntryPoint::Token(SyntaxKind::StringLiteral)
    );
    assert!(!string_result.statistics().full_fallback());
    assert_full_parse_equivalent(
        string_result.parse(),
        string_result.source(),
        parsed.limits(),
    );

    let (start, end) = byte_range(string_result.source().text(), "ok");
    let edit = TextEdit::new(string_result.source(), start, end, "safe").unwrap();
    let bytes_result = reparse(string_result.parse(), string_result.source(), &edit).unwrap();
    assert_eq!(
        bytes_result.statistics().entry_point(),
        ReparseEntryPoint::Token(SyntaxKind::BytesLiteral)
    );
    assert!(!bytes_result.statistics().full_fallback());
    assert_full_parse_equivalent(bytes_result.parse(), bytes_result.source(), parsed.limits());
}

#[test]
fn text_sensitive_identifiers_and_lexical_boundaries_fallback() {
    let text = concat!(
        "fn main() returns String {\n",
        "  /* nested /* block */ comment */\n",
        "  let value = 1;\n",
        "  let text = `hello ${value}`;\n",
        "  // line comment\n",
        "  `done`\n",
        "}\n",
    );
    let source = SourceFile::new(SourceFileId::new(2), text).unwrap();
    let parsed = parse(&source);
    assert!(!parsed.has_errors());

    let cases = [
        (byte_range(text, "value"), "Some"),
        (byte_range(text, "/*"), "/"),
        (byte_range(text, "${"), "$"),
        (byte_range(text, "}`"), "}"),
        (byte_range(text, "//"), "/"),
        (byte_range(text, "("), ""),
        (byte_range(text, "`hello"), "hello"),
    ];
    for ((start, end), replacement) in cases {
        let edit = TextEdit::new(&source, start, end, replacement).unwrap();
        let result = reparse(&parsed, &source, &edit).unwrap();
        assert!(
            result.statistics().full_fallback(),
            "unsafe edit {start}..{end} unexpectedly reparsed locally"
        );
        assert_eq!(result.statistics().entry_point(), ReparseEntryPoint::Source);
        assert_full_parse_equivalent(result.parse(), result.source(), parsed.limits());
    }

    let boundary = text.find("main").unwrap();
    let edit = TextEdit::new(&source, boundary, boundary, "x").unwrap();
    let result = reparse(&parsed, &source, &edit).unwrap();
    assert_eq!(
        result.statistics().fallback(),
        Some(ReparseFallback::EditCrossesTokenBoundary)
    );
    assert_full_parse_equivalent(result.parse(), result.source(), parsed.limits());
}

#[test]
fn exhaustive_focused_utf8_edits_match_full_parsing() {
    let text = "fn f(x: Int) returns String {\n  let crab = \"🦀e\u{301}海\";\n  `v=${x + 1}`\n}\n";
    let source = SourceFile::new(SourceFileId::new(3), text).unwrap();
    let parsed = parse(&source);
    let boundaries: Vec<_> = (0..=text.len())
        .filter(|offset| text.is_char_boundary(*offset))
        .collect();
    let replacements = ["", "x", " ", "🦀", "`", "/*", "}", "\r\n"];

    for &offset in &boundaries {
        for replacement in replacements {
            let edit = TextEdit::new(&source, offset, offset, replacement).unwrap();
            let result = reparse(&parsed, &source, &edit).unwrap();
            assert_full_parse_equivalent(result.parse(), result.source(), parsed.limits());
        }
    }
    for pair in boundaries.windows(2) {
        for replacement in replacements {
            let edit = TextEdit::new(&source, pair[0], pair[1], replacement).unwrap();
            let result = reparse(&parsed, &source, &edit).unwrap();
            assert_full_parse_equivalent(result.parse(), result.source(), parsed.limits());
        }
    }
}

#[test]
fn deterministic_random_valid_edit_sequence_matches_after_every_step() {
    let mut text = String::from("fn main() returns Int {\n");
    for index in 0..64 {
        text.push_str(&format!("  let value{index} = {index};\n"));
    }
    text.push_str("  value63\n}\n");
    let mut source = SourceFile::new(SourceFileId::new(4), text).unwrap();
    let mut parsed = parse(&source);
    let replacements = [" ", "  ", "\t", " \t "];
    let mut state = 0x005e_ed22_u64;

    for _ in 0..512 {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        let trivia: Vec<_> = parsed
            .syntax()
            .descendants_with_tokens()
            .filter_map(rowan::NodeOrToken::into_token)
            .filter(|token| token.kind() == SyntaxKind::Whitespace)
            .collect();
        let token_index = usize::try_from(state % trivia.len() as u64).unwrap();
        let replacement_index = usize::try_from((state >> 32) % replacements.len() as u64).unwrap();
        let token = &trivia[token_index];
        let replacement = replacements[replacement_index];
        let range = token.text_range();
        let edit = TextEdit::new(
            &source,
            u32::from(range.start()) as usize,
            u32::from(range.end()) as usize,
            replacement,
        )
        .unwrap();
        let result = reparse(&parsed, &source, &edit).unwrap();
        assert!(!result.statistics().full_fallback());
        assert_full_parse_equivalent(result.parse(), result.source(), parsed.limits());
        (source, parsed, _) = result.into_parts();
    }

    let printed = parsed.syntax().to_string();
    let printed_source = SourceFile::new(source.id(), printed).unwrap();
    let reparsed = parse(&printed_source);
    assert_eq!(parsed.green(), reparsed.green());
    assert_eq!(parsed.diagnostics(), reparsed.diagnostics());
}

#[test]
fn recovery_and_resource_limited_trees_always_fallback_with_exact_limits() {
    let source = SourceFile::new(SourceFileId::new(5), "fn f() {\n  let value = ;\n}\n").unwrap();
    let parsed = parse(&source);
    assert!(parsed.has_errors());
    let (start, end) = byte_range(source.text(), "  ");
    let edit = TextEdit::new(&source, start, end, "\t").unwrap();
    let result = reparse(&parsed, &source, &edit).unwrap();
    assert_eq!(
        result.statistics().fallback(),
        Some(ReparseFallback::PreviousParseHasErrors)
    );
    assert_full_parse_equivalent(result.parse(), result.source(), parsed.limits());

    let limit_cases = [
        SyntaxLimits {
            source_bytes: 8,
            ..SyntaxLimits::DEFAULT
        },
        SyntaxLimits {
            tokens: 2,
            ..SyntaxLimits::DEFAULT
        },
        SyntaxLimits {
            lexer_mode_depth: 1,
            ..SyntaxLimits::DEFAULT
        },
        SyntaxLimits {
            interpolation_brace_depth: 0,
            ..SyntaxLimits::DEFAULT
        },
        SyntaxLimits {
            parser_recursion: 1,
            ..SyntaxLimits::DEFAULT
        },
        SyntaxLimits {
            delimiter_depth: 1,
            ..SyntaxLimits::DEFAULT
        },
        SyntaxLimits {
            events: 2,
            ..SyntaxLimits::DEFAULT
        },
        SyntaxLimits {
            nodes: 1,
            ..SyntaxLimits::DEFAULT
        },
        SyntaxLimits {
            diagnostics: 1,
            ..SyntaxLimits::DEFAULT
        },
    ];
    for limits in limit_cases {
        let limited = parse_with_limits(&source, limits);
        let result = reparse(&limited, &source, &edit).unwrap();
        assert!(result.statistics().full_fallback());
        assert_full_parse_equivalent(result.parse(), result.source(), limited.limits());
    }
}

#[test]
fn stale_same_identity_snapshot_falls_back_instead_of_reusing_structure() {
    let original = SourceFile::new(SourceFileId::new(10), "fn f() returns Int { 1 }\n").unwrap();
    let parsed = parse(&original);
    let current = SourceFile::new(SourceFileId::new(10), "fn f() returns Int { 2 }\n").unwrap();
    let (start, end) = byte_range(current.text(), " ");
    let edit = TextEdit::new(&current, start, end, "\t").unwrap();
    let result = reparse(&parsed, &current, &edit).unwrap();
    assert_eq!(
        result.statistics().fallback(),
        Some(ReparseFallback::PreviousTreeDoesNotMatchSource)
    );
    assert_full_parse_equivalent(result.parse(), result.source(), parsed.limits());
}
