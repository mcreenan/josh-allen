use allen_syntax::{SourceFile, SourceFileId, SyntaxKind, lex, parse};
use std::{
    fs,
    path::{Path, PathBuf},
};

const FIXTURES: &[&str] = &[
    "comments.allen",
    "control-flow-and-errors.allen",
    "current.allen",
    "incomplete.allen",
    "l1-language.allen",
    "l2-language.allen",
    "l3-language.allen",
    "no-exception-keywords.allen",
    "operators.allen",
    "reserved-future-syntax.allen",
    "spec-preview.allen",
    "template-resources.allen",
    "template-strings.allen",
    "templates.allen",
    "unterminated-comment.allen",
    "unterminated-interpolation.allen",
];

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../editors/vscode/fixtures")
}

#[test]
fn every_editor_fixture_is_canonical_syntax_validated() {
    let directory = fixture_dir();
    let mut discovered: Vec<_> = fs::read_dir(&directory)
        .expect("editor fixture directory")
        .map(|entry| {
            entry
                .expect("editor fixture entry")
                .file_name()
                .into_string()
                .expect("UTF-8 editor fixture name")
        })
        .filter(|name| {
            Path::new(name)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("allen"))
        })
        .collect();
    discovered.sort();
    assert_eq!(
        discovered, FIXTURES,
        "fixture inventory must remain explicit"
    );

    let mut clean = 0;
    let mut recovered = 0;
    for (index, fixture) in FIXTURES.iter().enumerate() {
        let text = fs::read_to_string(directory.join(fixture)).expect("UTF-8 editor fixture");
        let source = SourceFile::new(
            SourceFileId::new(u32::try_from(index).unwrap()),
            text.clone(),
        )
        .unwrap();
        let lexed = lex(&source);
        assert_eq!(
            lexed.round_trip(&source),
            text,
            "{fixture}: lexer round trip"
        );
        assert_eq!(
            lexed.tokens().last().map(|token| token.kind()),
            Some(SyntaxKind::Eof),
            "{fixture}: lexer EOF"
        );

        let first = parse(&source);
        let second = parse(&source);
        first.assert_round_trip(&source);
        assert_eq!(first.green(), second.green(), "{fixture}: stable tree");
        assert_eq!(
            first.diagnostics(),
            second.diagnostics(),
            "{fixture}: stable ordered diagnostics"
        );

        let printed = first.syntax().to_string();
        let printed_source = SourceFile::new(source.id(), printed).unwrap();
        let printed_parse = parse(&printed_source);
        assert_eq!(
            first.green(),
            printed_parse.green(),
            "{fixture}: print tree"
        );
        assert_eq!(
            first.diagnostics(),
            printed_parse.diagnostics(),
            "{fixture}: print diagnostics"
        );
        if first.has_errors() {
            recovered += 1;
        } else {
            clean += 1;
        }
    }
    assert!(clean > 0, "editor corpus must retain valid fixtures");
    assert!(recovered > 0, "editor corpus must retain recovery fixtures");
}
