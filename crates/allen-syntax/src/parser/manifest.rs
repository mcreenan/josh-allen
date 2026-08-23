use super::{Parser, is_trivia};
use crate::SyntaxKind;

const MANIFEST_FIELDS: &[(&str, SyntaxKind)] = &[
    ("language", SyntaxKind::KwLanguage),
    ("entry", SyntaxKind::KwEntry),
    ("capabilities", SyntaxKind::KwCapabilities),
    ("http_origins", SyntaxKind::KwHttpOrigins),
    ("tools", SyntaxKind::KwTools),
];

impl Parser<'_, '_> {
    pub(super) fn inline_manifest(&mut self) {
        let marker = self.start();
        self.expect(SyntaxKind::KwManifest, "expected `manifest`");
        if !self.expect_open_delimiter(SyntaxKind::LBrace, "expected `{` after `manifest`") {
            self.complete(marker, SyntaxKind::InlineManifest);
            return;
        }

        let mut saw_field = false;
        while !self.exceeded
            && !self.at(SyntaxKind::RBrace)
            && !self.at(SyntaxKind::Eof)
            && !self.at_manifest_top_level_sync()
        {
            if self.at_manifest_field() {
                self.manifest_field();
                saw_field = true;
            } else {
                self.error_until_manifest_field();
            }

            if self.at(SyntaxKind::Comma) {
                self.bump_nontrivia();
            } else if !self.at_manifest_field()
                && !self.at(SyntaxKind::RBrace)
                && !self.at(SyntaxKind::Eof)
                && !self.at_manifest_top_level_sync()
            {
                self.error_until_manifest_field();
            }
        }
        if !saw_field {
            self.missing("expected manifest field");
        }
        self.expect_close_delimiter(SyntaxKind::RBrace, "expected `}` after inline manifest");
        self.complete(marker, SyntaxKind::InlineManifest);
    }

    fn manifest_field(&mut self) {
        let marker = self.start();
        if self.at_contextual("language") {
            self.bump_contextual(SyntaxKind::KwLanguage);
            self.expect(SyntaxKind::Colon, "expected `:` after `language`");
            self.expect(
                SyntaxKind::StringLiteral,
                "expected language version string",
            );
        } else if self.at_contextual("entry") {
            self.bump_contextual(SyntaxKind::KwEntry);
            self.expect(SyntaxKind::Colon, "expected `:` after `entry`");
            self.expect(SyntaxKind::Ident, "expected entry function name");
        } else if self.at_contextual("capabilities") {
            self.bump_contextual(SyntaxKind::KwCapabilities);
            self.expect(SyntaxKind::Colon, "expected `:` after `capabilities`");
            self.capability_list();
        } else if self.at_contextual("http_origins") {
            self.bump_contextual(SyntaxKind::KwHttpOrigins);
            self.expect(SyntaxKind::Colon, "expected `:` after `http_origins`");
            self.http_origin_list();
        } else if self.at_contextual("tools") {
            self.bump_contextual(SyntaxKind::KwTools);
            self.expect(SyntaxKind::Colon, "expected `:` after `tools`");
            self.tools_field();
        } else {
            self.missing("expected manifest field");
        }
        self.complete(marker, SyntaxKind::ManifestField);
    }

    fn capability_list(&mut self) {
        if !self.expect_open_delimiter(SyntaxKind::LBracket, "expected `[` before capability list")
        {
            return;
        }
        let mut need_capability = !self.at(SyntaxKind::RBracket);
        while !self.exceeded
            && !self.at(SyntaxKind::RBracket)
            && !self.at(SyntaxKind::RBrace)
            && !self.at(SyntaxKind::Eof)
            && !self.at_manifest_top_level_sync()
        {
            if need_capability {
                self.capability();
                need_capability = false;
            }
            if self.at(SyntaxKind::Comma) {
                self.bump_nontrivia();
                need_capability = !self.at(SyntaxKind::RBracket);
            } else if !self.at(SyntaxKind::RBracket) {
                self.missing("expected `,` between capabilities");
                need_capability = true;
            }
        }
        self.expect_close_delimiter(SyntaxKind::RBracket, "expected `]` after capability list");
    }

    fn capability(&mut self) {
        let marker = self.start();
        self.effect_id();
        if self.at(SyntaxKind::LParen)
            && self.expect_open_delimiter(
                SyntaxKind::LParen,
                "expected `(` before capability argument",
            )
        {
            self.expect(SyntaxKind::Ident, "expected capability argument");
            self.expect_close_delimiter(
                SyntaxKind::RParen,
                "expected `)` after capability argument",
            );
        }
        self.complete(marker, SyntaxKind::Capability);
    }

    pub(super) fn effect_id(&mut self) {
        self.eat_trivia();
        let count = self.effect_id_candidate_len();
        if count == 0 {
            if self.at_capability_end() {
                self.missing("expected effect ID");
            } else {
                self.error_one("expected canonical effect ID");
            }
            return;
        }

        let start = self.pos;
        let end = start + count;
        if self.effect_id_candidate_is_canonical(start, end) {
            self.token_as(count, Some(SyntaxKind::EffectId));
        } else {
            let error = self.start();
            for _ in 0..count {
                self.bump();
            }
            self.complete(error, SyntaxKind::Error);
            self.error("expected canonical effect ID");
        }
    }

    fn http_origin_list(&mut self) {
        if !self.expect_open_delimiter(SyntaxKind::LBracket, "expected `[` before HTTP origin list")
        {
            return;
        }
        let mut need_origin = !self.at(SyntaxKind::RBracket);
        while !self.exceeded
            && !self.at(SyntaxKind::RBracket)
            && !self.at(SyntaxKind::RBrace)
            && !self.at(SyntaxKind::Eof)
            && !self.at_manifest_top_level_sync()
        {
            if need_origin {
                if !self.expect(SyntaxKind::StringLiteral, "expected HTTP origin string") {
                    self.recover_list_item();
                }
                need_origin = false;
            }
            if self.at(SyntaxKind::Comma) {
                self.bump_nontrivia();
                need_origin = !self.at(SyntaxKind::RBracket);
            } else if !self.at(SyntaxKind::RBracket) {
                self.missing("expected `,` between HTTP origins");
                need_origin = true;
            }
        }
        self.expect_close_delimiter(SyntaxKind::RBracket, "expected `]` after HTTP origin list");
    }

    fn tools_field(&mut self) {
        if !self.expect_open_delimiter(SyntaxKind::LBrace, "expected `{` after `tools:`") {
            return;
        }
        if self.at_contextual("required") {
            self.bump_contextual(SyntaxKind::KwRequired);
        } else {
            self.missing("expected `required` in tools manifest");
        }
        self.expect(SyntaxKind::Colon, "expected `:` after `required`");
        self.tool_requirement_list();
        if self.at(SyntaxKind::Comma) {
            self.bump_nontrivia();
        }
        self.expect_close_delimiter(SyntaxKind::RBrace, "expected `}` after tools manifest");
    }

    fn tool_requirement_list(&mut self) {
        if !self.expect_open_delimiter(SyntaxKind::LBracket, "expected `[` before required tools") {
            return;
        }
        let mut need_requirement = !self.at(SyntaxKind::RBracket);
        while !self.exceeded
            && !self.at(SyntaxKind::RBracket)
            && !self.at(SyntaxKind::RBrace)
            && !self.at(SyntaxKind::Eof)
            && !self.at_manifest_top_level_sync()
        {
            if need_requirement {
                if self.at(SyntaxKind::LBrace) {
                    self.tool_requirement();
                } else {
                    self.error_one("expected required tool record");
                }
                need_requirement = false;
            }
            if self.at(SyntaxKind::Comma) {
                self.bump_nontrivia();
                need_requirement = !self.at(SyntaxKind::RBracket);
            } else if !self.at(SyntaxKind::RBracket) {
                self.missing("expected `,` between required tools");
                need_requirement = true;
            }
        }
        self.expect_close_delimiter(SyntaxKind::RBracket, "expected `]` after required tools");
    }

    fn tool_requirement(&mut self) {
        let marker = self.start();
        if !self.expect_open_delimiter(SyntaxKind::LBrace, "expected `{` before required tool") {
            self.complete(marker, SyntaxKind::ToolRequirement);
            return;
        }
        self.expect_contextual(
            "name",
            SyntaxKind::KwName,
            "expected `name` in required tool",
        );
        self.expect(SyntaxKind::Colon, "expected `:` after tool `name`");
        self.expect(SyntaxKind::StringLiteral, "expected tool name string");
        self.expect(
            SyntaxKind::Comma,
            "expected `,` between tool name and version",
        );
        self.expect_contextual(
            "version",
            SyntaxKind::KwVersion,
            "expected `version` in required tool",
        );
        self.expect(SyntaxKind::Colon, "expected `:` after tool `version`");
        self.expect(SyntaxKind::StringLiteral, "expected tool version string");
        if self.at(SyntaxKind::Comma) {
            self.bump_nontrivia();
        }
        self.expect_close_delimiter(SyntaxKind::RBrace, "expected `}` after required tool");
        self.complete(marker, SyntaxKind::ToolRequirement);
    }

    fn at_manifest_field(&self) -> bool {
        MANIFEST_FIELDS
            .iter()
            .any(|(spelling, _)| self.at_contextual(spelling))
    }

    fn at_contextual(&self, spelling: &str) -> bool {
        let Some((_, token)) = self.nth_token(0) else {
            return false;
        };
        token.kind() == SyntaxKind::Ident && token.text(self.source) == spelling
    }

    fn expect_contextual(
        &mut self,
        spelling: &str,
        kind: SyntaxKind,
        message: &'static str,
    ) -> bool {
        if self.at_contextual(spelling) {
            self.bump_contextual(kind);
            true
        } else {
            self.missing(message);
            false
        }
    }

    fn bump_contextual(&mut self, kind: SyntaxKind) {
        self.eat_trivia();
        self.token_as(1, Some(kind));
    }

    fn nth_token(&self, mut nontrivia: usize) -> Option<(usize, crate::LexToken)> {
        let mut index = self.pos;
        while let Some(token) = self.tokens.get(index).copied() {
            if !is_trivia(token.kind()) {
                if nontrivia == 0 {
                    return Some((index, token));
                }
                nontrivia -= 1;
            }
            index += 1;
        }
        None
    }

    fn effect_id_candidate_len(&self) -> usize {
        let mut index = self.pos;
        let mut previous_end = None;
        while let Some(token) = self.tokens.get(index).copied() {
            if token.kind() == SyntaxKind::Eof || is_trivia(token.kind()) {
                break;
            }
            let start = token.range().start();
            if previous_end.is_some_and(|end| end != start) {
                break;
            }
            let text = token.text(self.source);
            if !is_effect_component(token.kind(), text) {
                break;
            }
            previous_end = Some(token.range().end());
            index += 1;
        }
        index - self.pos
    }

    fn effect_id_candidate_is_canonical(&self, start: usize, end: usize) -> bool {
        let Some(first) = self.tokens.get(start).copied() else {
            return false;
        };
        let Some(last) = self.tokens.get(end.saturating_sub(1)).copied() else {
            return false;
        };
        let start = u32::from(first.range().start()) as usize;
        let end = u32::from(last.range().end()) as usize;
        self.source
            .text()
            .get(start..end)
            .is_some_and(is_canonical_effect_id)
    }

    fn at_capability_end(&self) -> bool {
        matches!(
            self.nth(0),
            SyntaxKind::Comma | SyntaxKind::RBracket | SyntaxKind::RBrace | SyntaxKind::Eof
        )
    }

    fn recover_list_item(&mut self) {
        while !self.exceeded
            && !matches!(
                self.nth(0),
                SyntaxKind::Comma | SyntaxKind::RBracket | SyntaxKind::RBrace | SyntaxKind::Eof
            )
            && !self.at_manifest_top_level_sync()
        {
            self.error_one("unexpected list item syntax");
        }
    }

    fn error_until_manifest_field(&mut self) {
        let marker = self.start();
        let mut consumed = false;
        while !self.exceeded
            && !self.at_manifest_field()
            && !matches!(
                self.nth(0),
                SyntaxKind::Comma | SyntaxKind::RBrace | SyntaxKind::Eof
            )
            && !self.at_manifest_top_level_sync()
        {
            self.bump_nontrivia();
            consumed = true;
        }
        if consumed {
            self.complete(marker, SyntaxKind::Error);
            self.error("unexpected inline manifest syntax");
        } else {
            self.complete(marker, SyntaxKind::Missing);
            self.error("expected manifest field");
            if self.at(SyntaxKind::Comma) {
                self.bump_nontrivia();
            }
        }
    }

    fn at_manifest_top_level_sync(&self) -> bool {
        match self.nth(0) {
            SyntaxKind::KwImport | SyntaxKind::KwManifest => self.nth(1) == SyntaxKind::LBrace,
            SyntaxKind::KwRecord | SyntaxKind::KwEnum | SyntaxKind::KwType | SyntaxKind::KwFn => {
                self.nth(1) == SyntaxKind::Ident
            }
            SyntaxKind::KwAsync => self.nth(1) == SyntaxKind::KwFn,
            SyntaxKind::KwExport => matches!(
                self.nth(1),
                SyntaxKind::KwRecord
                    | SyntaxKind::KwEnum
                    | SyntaxKind::KwType
                    | SyntaxKind::KwFn
                    | SyntaxKind::KwAsync
            ),
            _ => false,
        }
    }
}

fn is_effect_component(kind: SyntaxKind, text: &str) -> bool {
    kind != SyntaxKind::ErrorToken
        && (matches!(
            kind,
            SyntaxKind::Dot | SyntaxKind::At | SyntaxKind::IntLiteral
        ) || text.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
            && text
                .as_bytes()
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_'))
}

fn is_canonical_effect_id(text: &str) -> bool {
    let (segments, major) = text
        .split_once('@')
        .map_or((text, None), |(segments, major)| (segments, Some(major)));
    if text.matches('@').count() > 1 || !segments.split('.').all(is_canonical_effect_segment) {
        return false;
    }
    major.is_none_or(|major| {
        major.as_bytes().first().is_some_and(|byte| *byte != b'0')
            && major.as_bytes().iter().all(u8::is_ascii_digit)
    })
}

fn is_canonical_effect_segment(segment: &str) -> bool {
    segment
        .as_bytes()
        .split_first()
        .is_some_and(|(first, rest)| {
            first.is_ascii_lowercase()
                && rest
                    .iter()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SourceFile, SourceFileId, parse};

    fn source(text: &str) -> SourceFile {
        SourceFile::new(SourceFileId::new(22), text).unwrap()
    }

    fn node_kinds(text: &str) -> Vec<SyntaxKind> {
        parse(&source(text))
            .syntax()
            .descendants()
            .map(|node| node.kind())
            .collect()
    }

    fn token_kinds_and_text(text: &str) -> Vec<(SyntaxKind, String)> {
        parse(&source(text))
            .syntax()
            .descendants_with_tokens()
            .filter_map(rowan::NodeOrToken::into_token)
            .map(|token| (token.kind(), token.text().to_owned()))
            .collect()
    }

    #[test]
    fn complete_manifest_grammar_builds_generated_nodes_and_contextual_tokens() {
        let text = r#"manifest {
  language: "0.1"
  entry: main,
  capabilities: [fs.read(workdir), tool.github.create_issue@2, record.read,]
  http_origins: ["https://example.test",]
  tools: { required: [
    { name: "github.create_issue", version: ">=2.0.0, <3.0.0", },
  ], }
}
export fn main() returns Void {}"#;
        let source = source(text);
        let parsed = parse(&source);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        assert!(!parsed.has_errors());
        parsed.assert_round_trip(&source);

        let kinds: Vec<_> = parsed
            .syntax()
            .descendants()
            .map(|node| node.kind())
            .collect();
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == SyntaxKind::ManifestField)
                .count(),
            5
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == SyntaxKind::Capability)
                .count(),
            3
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == SyntaxKind::ToolRequirement)
                .count(),
            1
        );

        let tokens = token_kinds_and_text(text);
        for (spelling, kind) in [
            ("language", SyntaxKind::KwLanguage),
            ("entry", SyntaxKind::KwEntry),
            ("capabilities", SyntaxKind::KwCapabilities),
            ("http_origins", SyntaxKind::KwHttpOrigins),
            ("tools", SyntaxKind::KwTools),
            ("required", SyntaxKind::KwRequired),
            ("name", SyntaxKind::KwName),
            ("version", SyntaxKind::KwVersion),
        ] {
            assert!(tokens.contains(&(kind, spelling.to_owned())));
        }
        assert!(tokens.contains(&(
            SyntaxKind::EffectId,
            "tool.github.create_issue@2".to_owned()
        )));
    }

    #[test]
    fn contextual_spellings_remain_identifiers_outside_manifest_positions() {
        let text =
            "record language { entry: tools } fn required(name: version) returns capabilities {}";
        let parsed = parse(&source(text));
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        let tokens = token_kinds_and_text(text);
        for spelling in [
            "language",
            "entry",
            "tools",
            "required",
            "name",
            "version",
            "capabilities",
        ] {
            assert!(tokens.contains(&(SyntaxKind::Ident, spelling.to_owned())));
        }
    }

    #[test]
    fn rejects_noncanonical_or_spaced_effect_ids_without_losing_text() {
        for effect in [
            "Tool.github.create_issue@2",
            "tool.Github.create_issue@2",
            "tool.github.create_issue@0",
            "tool.github.create_issue@01",
            "tool.github.create_issue @2",
            "tool.github. create_issue@2",
        ] {
            let text = format!("manifest {{ capabilities: [{effect}] }}");
            let source = source(&text);
            let parsed = parse(&source);
            assert!(parsed.has_errors(), "accepted invalid effect ID {effect}");
            parsed.assert_round_trip(&source);
        }
    }

    #[test]
    fn effect_major_versions_are_unbounded_positive_decimal_components() {
        let major = "999999999999999999999999999999999999999999999999";
        let effect = format!("tool.release@{major}");
        let text = format!(
            "manifest {{ capabilities: [{effect}] }} fn f() returns Void effects [{effect}] {{}}"
        );
        let file = source(&text);
        let parsed = parse(&file);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        assert!(!parsed.has_errors());
        parsed.assert_round_trip(&file);
        assert_eq!(
            parsed
                .syntax()
                .descendants_with_tokens()
                .filter_map(rowan::NodeOrToken::into_token)
                .filter(|token| token.kind() == SyntaxKind::EffectId && token.text() == effect)
                .count(),
            2
        );

        let ordinary =
            source("fn f() returns Int { 999999999999999999999999999999999999999999999999 }");
        let parsed = parse(&ordinary);
        assert!(parsed.has_errors());
        assert!(
            parsed
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.message() == "integer literal exceeds Int range")
        );
        parsed.assert_round_trip(&ordinary);
    }

    #[test]
    fn rejects_manifest_shapes_outside_the_exact_productions() {
        for text in [
            "manifest { unknown: \"value\" }",
            "manifest { http_origins: [\"https://one.test\" \"https://two.test\"] }",
            "manifest { tools: { other: [] } }",
            "manifest { tools: { required: [{ version: \"2\", name: \"github\" }] } }",
            "manifest { tools: { required: [{ name: \"github\" version: \"2\" }] } }",
        ] {
            let source = source(text);
            let parsed = parse(&source);
            assert!(parsed.has_errors(), "accepted invalid manifest {text}");
            parsed.assert_round_trip(&source);
        }
    }

    #[test]
    fn invalid_manifest_recovers_to_later_declarations() {
        let text = "manifest { capabilities: [tool.bad@0, fs.read workdir] unknown: [] record Later { value: T } fn recovered() returns Void {}";
        let source = source(text);
        let parsed = parse(&source);
        assert!(parsed.has_errors());
        let kinds = node_kinds(text);
        assert!(kinds.contains(&SyntaxKind::Error));
        assert!(kinds.contains(&SyntaxKind::Missing));
        assert!(kinds.contains(&SyntaxKind::RecordDeclaration));
        assert!(kinds.contains(&SyntaxKind::FunctionDeclaration));
        parsed.assert_round_trip(&source);
    }

    #[test]
    fn grammar_does_not_add_manifest_uniqueness_policy() {
        let text = "manifest { language: \"0.1\" language: \"0.1\" entry: main entry: main capabilities: [] capabilities: [] }";
        let source = source(text);
        let parsed = parse(&source);
        assert!(
            parsed.diagnostics().is_empty(),
            "{:?}",
            parsed.diagnostics()
        );
        assert!(!parsed.has_errors());
        parsed.assert_round_trip(&source);
    }

    #[test]
    fn malformed_manifest_obeys_parser_resource_fallbacks() {
        let text = "manifest { capabilities: [tool.bad@0 tool.bad@01 Tool.bad] tools: { required: [{ version: 1 }] } record Later { value: T }";
        let source = source(text);
        for limits in [
            crate::SyntaxLimits {
                events: 8,
                ..crate::SyntaxLimits::DEFAULT
            },
            crate::SyntaxLimits {
                nodes: 4,
                ..crate::SyntaxLimits::DEFAULT
            },
            crate::SyntaxLimits {
                diagnostics: 1,
                ..crate::SyntaxLimits::DEFAULT
            },
        ] {
            let parsed = crate::parse_with_limits(&source, limits);
            assert_eq!(parsed.syntax().kind(), SyntaxKind::Source);
            assert!(parsed.has_errors());
            parsed.assert_round_trip(&source);
        }
    }

    #[test]
    fn lexer_limit_error_token_cannot_become_an_effect_id() {
        assert!(!is_effect_component(SyntaxKind::ErrorToken, "fs"));

        let source = source("manifest{capabilities:[fs");
        let limits = crate::SyntaxLimits {
            tokens: 6,
            diagnostics: 0,
            ..crate::SyntaxLimits::DEFAULT
        };
        let lexed = crate::lex_with_limits(&source, limits);
        assert!(
            lexed
                .tokens()
                .iter()
                .any(|token| token.kind() == SyntaxKind::ErrorToken)
        );

        let parsed = crate::parse_with_limits(&source, limits);
        assert!(parsed.diagnostics().is_empty());
        assert!(parsed.has_errors());
        assert!(
            parsed
                .syntax()
                .descendants_with_tokens()
                .filter_map(rowan::NodeOrToken::into_token)
                .any(|token| token.kind() == SyntaxKind::ErrorToken)
        );
        parsed.assert_round_trip(&source);
    }
}
