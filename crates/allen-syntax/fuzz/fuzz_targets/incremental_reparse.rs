#![no_main]

use allen_syntax::{SourceFile, SourceFileId, TextEdit, parse, reparse};
use libfuzzer_sys::fuzz_target;

const MAX_INPUT_BYTES: usize = 16 * 1024;
const MAX_EDITS_PER_INPUT: usize = 16;
const REPLACEMENTS: &[&str] = &["", " ", "  ", "x", "🦀", "`", "/*", "${", "}", "\r\n", "\""];

fuzz_target!(|data: &[u8]| {
    let data = &data[..data.len().min(MAX_INPUT_BYTES)];
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let mut source = SourceFile::new(SourceFileId::new(0), text).expect("bounded UTF-8 source");
    let mut parsed = parse(&source);

    for edit_index in 0..data.len().min(MAX_EDITS_PER_INPUT) {
        let boundaries: Vec<_> = (0..=source.text().len())
            .filter(|offset| source.text().is_char_boundary(*offset))
            .collect();
        let start_index = usize::from(data[edit_index]) % boundaries.len();
        let width = usize::from(data[(edit_index + 1) % data.len()].wrapping_add(1)) % 4;
        let end_index = (start_index + width).min(boundaries.len() - 1);
        let replacement =
            REPLACEMENTS[usize::from(data[(edit_index + 2) % data.len()]) % REPLACEMENTS.len()];
        let removed = boundaries[end_index] - boundaries[start_index];
        let Some(edited_len) = source
            .text()
            .len()
            .checked_sub(removed)
            .and_then(|length| length.checked_add(replacement.len()))
        else {
            break;
        };
        if edited_len > MAX_INPUT_BYTES {
            break;
        }

        let edit = TextEdit::new(
            &source,
            boundaries[start_index],
            boundaries[end_index],
            replacement,
        )
        .expect("UTF-8 boundaries were selected from the source");
        let first = reparse(&parsed, &source, &edit).expect("matching syntax source identity");
        let repeated = reparse(&parsed, &source, &edit).expect("deterministic repeated reparse");
        let fresh = parse(first.source());

        assert_eq!(first.source().text(), repeated.source().text());
        assert_eq!(first.parse().green(), repeated.parse().green());
        assert_eq!(first.parse().diagnostics(), repeated.parse().diagnostics());
        assert_eq!(first.statistics(), repeated.statistics());
        assert_eq!(first.parse().green(), fresh.green(), "incremental tree");
        assert_eq!(
            first.parse().diagnostics(),
            fresh.diagnostics(),
            "incremental ordered diagnostics",
        );
        assert_eq!(first.parse().has_errors(), fresh.has_errors());

        let printed = first.parse().syntax().to_string();
        let printed_source =
            SourceFile::new(first.source().id(), printed).expect("printed source range");
        let printed_parse = parse(&printed_source);
        assert_eq!(
            fresh.green(),
            printed_parse.green(),
            "parse/print/parse tree"
        );
        assert_eq!(
            fresh.diagnostics(),
            printed_parse.diagnostics(),
            "parse/print/parse diagnostics",
        );

        (source, parsed, _) = first.into_parts();
    }
});
