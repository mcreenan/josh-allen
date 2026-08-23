use allen_syntax::{
    Parse, SourceFile, SourceFileId, SyntaxKind, SyntaxNode, TextRange, lex, parse,
};
use std::{fmt::Write as _, path::Path};

const VALID_SOURCE: &str = include_str!("../test-data/golden/valid.allen");
const INVALID_SOURCE: &str = include_str!("../test-data/golden/invalid.allen");

struct Fixture {
    name: &'static str,
    source: &'static str,
    tokens: &'static str,
    tree: &'static str,
}

const FIXTURES: &[Fixture] = &[
    Fixture {
        name: "valid",
        source: VALID_SOURCE,
        tokens: include_str!("../test-data/golden/valid.tokens"),
        tree: include_str!("../test-data/golden/valid.tree"),
    },
    Fixture {
        name: "invalid",
        source: INVALID_SOURCE,
        tokens: include_str!("../test-data/golden/invalid.tokens"),
        tree: include_str!("../test-data/golden/invalid.tree"),
    },
];

fn offset(size: allen_syntax::TextSize) -> usize {
    u32::from(size) as usize
}

fn range_text(range: TextRange) -> String {
    format!("{}..{}", offset(range.start()), offset(range.end()))
}

fn into_token(element: allen_syntax::SyntaxElement) -> Option<allen_syntax::SyntaxToken> {
    element.into_token()
}

fn dump_lex(source: &SourceFile) -> String {
    let lexed = lex(source);
    let mut output = String::new();
    for token in lexed.tokens() {
        writeln!(
            output,
            "{:?} {} {:?}",
            token.kind(),
            range_text(token.range()),
            token.text(source)
        )
        .expect("writing to a String cannot fail");
    }
    output.push_str("diagnostics:\n");
    dump_diagnostics(&mut output, lexed.diagnostics());
    output
}

fn dump_parse(parse: &Parse) -> String {
    let mut output = String::new();
    dump_node(&mut output, &parse.syntax(), 0);
    output.push_str("diagnostics:\n");
    dump_diagnostics(&mut output, parse.diagnostics());
    output
}

fn dump_node(output: &mut String, node: &SyntaxNode, depth: usize) {
    writeln!(
        output,
        "{:indent$}{:?} {}",
        "",
        node.kind(),
        range_text(node.text_range()),
        indent = depth * 2
    )
    .expect("writing to a String cannot fail");
    for element in node.children_with_tokens() {
        if let Some(child) = element.as_node() {
            dump_node(output, child, depth + 1);
        } else if let Some(token) = element.as_token() {
            writeln!(
                output,
                "{:indent$}{:?} {} {:?}",
                "",
                token.kind(),
                range_text(token.text_range()),
                token.text(),
                indent = (depth + 1) * 2
            )
            .expect("writing to a String cannot fail");
        }
    }
}

fn dump_diagnostics(output: &mut String, diagnostics: &[allen_syntax::SyntaxDiagnostic]) {
    for diagnostic in diagnostics {
        writeln!(
            output,
            "{} {} {:?}",
            diagnostic.code(),
            range_text(diagnostic.range()),
            diagnostic.message()
        )
        .expect("writing to a String cannot fail");
    }
}

fn assert_golden(path: &Path, expected: &str, actual: &str) {
    if std::env::var_os("UPDATE_ALLEN_SYNTAX_GOLDENS").is_some() {
        std::fs::write(path, actual).expect("update syntax golden");
    } else {
        assert_eq!(actual, expected, "golden mismatch at {}", path.display());
    }
}

fn assert_source_range(source: &SourceFile, range: TextRange, context: &str) {
    assert!(
        source.text_at(range).is_some(),
        "{context} has an invalid UTF-8 source range {}",
        range_text(range)
    );
}

fn assert_lex_invariants(source: &SourceFile) {
    let lexed = lex(source);
    assert_eq!(lexed.round_trip(source), source.text());

    let mut cursor = 0;
    let mut eof_count = 0;
    for token in lexed.tokens() {
        let range = token.range();
        assert_source_range(source, range, "lexer token");
        assert_eq!(offset(range.start()), cursor, "lexer token gap or overlap");
        if token.kind() == SyntaxKind::Eof {
            eof_count += 1;
            assert!(range.is_empty(), "EOF must be zero-width");
            assert_eq!(cursor, source.text().len(), "EOF must be at source end");
            assert_eq!(token.text(source), "");
        } else {
            assert!(!range.is_empty(), "only EOF may be zero-width");
            cursor = offset(range.end());
        }
    }
    assert_eq!(cursor, source.text().len(), "tokens must cover all source");
    assert_eq!(eof_count, 1, "there must be exactly one EOF token");
    assert_eq!(
        lexed.tokens().last().map(|token| token.kind()),
        Some(SyntaxKind::Eof),
        "EOF must terminate the lexer stream"
    );
    for diagnostic in lexed.diagnostics() {
        assert_eq!(diagnostic.source(), source.id());
        assert_source_range(source, diagnostic.range(), "lexer diagnostic");
    }
}

fn assert_parse_invariants(source: &SourceFile, parsed: &Parse) {
    parsed.assert_round_trip(source);
    let root = parsed.syntax();
    assert_eq!(root.kind(), SyntaxKind::Source);
    assert_eq!(offset(root.text_range().start()), 0);
    assert_eq!(offset(root.text_range().end()), source.text().len());

    for node in root.descendants() {
        let range = node.text_range();
        assert_source_range(source, range, "syntax node");
        if let Some(parent) = node.parent() {
            let parent_range = parent.text_range();
            assert!(
                parent_range.start() <= range.start() && range.end() <= parent_range.end(),
                "child node range must be contained by its parent"
            );
        }
    }

    let tokens: Vec<_> = root
        .descendants_with_tokens()
        .filter_map(into_token)
        .collect();
    let mut cursor = 0;
    let mut eof_count = 0;
    for token in &tokens {
        let range = token.text_range();
        assert_source_range(source, range, "syntax token");
        assert_eq!(offset(range.start()), cursor, "tree token gap or overlap");
        if token.kind() == SyntaxKind::Eof {
            eof_count += 1;
            assert!(range.is_empty(), "tree EOF must be zero-width");
            assert!(token.text().is_empty());
            assert_eq!(
                cursor,
                source.text().len(),
                "tree EOF must be at source end"
            );
        } else {
            assert!(!range.is_empty(), "only tree EOF may be zero-width");
            cursor = offset(range.end());
        }
    }
    assert_eq!(cursor, source.text().len(), "tree must cover all source");
    assert_eq!(eof_count, 1, "tree must contain exactly one EOF token");
    assert_eq!(
        tokens.last().map(allen_syntax::SyntaxToken::kind),
        Some(SyntaxKind::Eof),
        "EOF must terminate the tree token stream"
    );
    for diagnostic in parsed.diagnostics() {
        assert_eq!(diagnostic.source(), source.id());
        assert_source_range(source, diagnostic.range(), "parse diagnostic");
    }
}

#[test]
fn fixture_token_and_tree_streams_match_checked_in_goldens() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (index, fixture) in FIXTURES.iter().enumerate() {
        let fixture_id = u32::try_from(index).expect("fixture count fits a source ID");
        let source = SourceFile::new(SourceFileId::new(fixture_id), fixture.source)
            .expect("small checked-in fixture");
        let golden_dir = manifest_dir.join("test-data/golden");
        assert_golden(
            &golden_dir.join(format!("{}.tokens", fixture.name)),
            fixture.tokens,
            &dump_lex(&source),
        );
        let parsed = parse(&source);
        assert_golden(
            &golden_dir.join(format!("{}.tree", fixture.name)),
            fixture.tree,
            &dump_parse(&parsed),
        );
        assert_lex_invariants(&source);
        assert_parse_invariants(&source, &parsed);
    }
}

#[test]
fn fixtures_pin_top_level_recovery_and_later_declarations() {
    let valid = SourceFile::new(SourceFileId::new(10), VALID_SOURCE).unwrap();
    let valid_parse = parse(&valid);
    assert!(valid_parse.diagnostics().is_empty());
    assert!(!valid_parse.has_errors());

    let invalid = SourceFile::new(SourceFileId::new(11), INVALID_SOURCE).unwrap();
    let invalid_parse = parse(&invalid);
    assert!(invalid_parse.has_errors());
    let kinds: Vec<_> = invalid_parse
        .syntax()
        .descendants()
        .map(|node| node.kind())
        .collect();
    assert!(kinds.contains(&SyntaxKind::Error));
    assert!(kinds.contains(&SyntaxKind::RecordDeclaration));
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| **kind == SyntaxKind::FunctionDeclaration)
            .count(),
        2,
        "recovery must reach both later function declarations"
    );
}

#[test]
fn type_alias_declarations_cover_type_shapes_without_a_terminator() {
    let text = concat!(
        "type Scalar = Int\n",
        "type Collection = List<Scalar>\n",
        "type Pair = (Int, String)\n",
        "type Shape = { value: Scalar }\n",
        "export type Callback = fn(Int) returns String effects []\n",
        "export fn main() returns Scalar { 42 }\n",
    );
    let source = SourceFile::new(SourceFileId::new(30), text).unwrap();
    let parsed = parse(&source);
    assert!(
        parsed.diagnostics().is_empty(),
        "{:?}",
        parsed.diagnostics()
    );
    assert!(!parsed.has_errors());
    assert_eq!(
        parsed
            .syntax()
            .descendants()
            .filter(|node| node.kind() == SyntaxKind::TypeAliasDeclaration)
            .count(),
        5
    );
    assert_eq!(
        lex(&source)
            .tokens()
            .iter()
            .filter(|token| token.kind() == SyntaxKind::KwType)
            .count(),
        5
    );
}

#[test]
fn type_alias_recovery_preserves_following_declarations() {
    for (id, text) in [
        (31, "type MissingEquals Int\ntype Next = String\n"),
        (32, "type MissingTarget =\nrecord Next {}\n"),
        (33, "type Semicolon = Int;\ntype Next = String\n"),
        (34, "manifest { language: \"0.1\"\ntype Next = Int\n"),
        (35, "type Generic<T> = T\ntype Next = Int\n"),
    ] {
        let source = SourceFile::new(SourceFileId::new(id), text).unwrap();
        let parsed = parse(&source);
        assert!(parsed.has_errors(), "fixture should recover: {text}");
        let kinds = parsed
            .syntax()
            .descendants()
            .map(|node| node.kind())
            .collect::<Vec<_>>();
        assert!(
            kinds.contains(&SyntaxKind::TypeAliasDeclaration),
            "later alias should survive recovery: {text}"
        );
        if text.contains("record Next") {
            assert!(kinds.contains(&SyntaxKind::RecordDeclaration));
        }
    }
}

#[test]
fn every_utf8_boundary_prefix_terminates_and_round_trips() {
    for (fixture_index, fixture) in FIXTURES.iter().enumerate() {
        for end in (0..=fixture.source.len()).filter(|end| fixture.source.is_char_boundary(*end)) {
            let prefix = &fixture.source[..end];
            let fixture_id = u32::try_from(fixture_index).expect("fixture count fits a source ID");
            let source = SourceFile::new(SourceFileId::new(fixture_id), prefix)
                .expect("fixture prefix fits the syntax range model");
            assert_lex_invariants(&source);
            let parsed = parse(&source);
            assert_parse_invariants(&source, &parsed);
        }
    }
}
