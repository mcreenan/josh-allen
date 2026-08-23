//! Lossless syntax infrastructure for the ALLEN language.
//!
//! This crate is intentionally independent from compiler semantics, bytecode,
//! packages, runtimes, and hosts. The production compiler consumes this public
//! syntax facade without adding compiler semantics to concrete syntax nodes.

#![forbid(unsafe_code)]

mod diagnostic;
mod incremental;
mod lexer;
mod parser;
mod source;
mod tree_sink;

#[allow(clippy::all, clippy::pedantic)]
mod generated {
    include!("generated/kinds.rs");
    include!("generated/ast.rs");
    include!("generated/inventory.rs");
}

pub use diagnostic::{SyntaxDiagnostic, SyntaxDiagnosticError, SyntaxDiagnosticLabel};
pub use generated::*;
pub use incremental::{
    IncrementalParse, ReparseEntryPoint, ReparseFallback, ReparseStatistics, TextEdit,
    TextEditError, reparse,
};
pub use lexer::{LexToken, Lexed, SyntaxLimits, lex, lex_with_limits};
pub use parser::{Parse, parse, parse_with_limits};
pub use rowan::ast::{AstChildren, AstNode};
pub use rowan::{GreenNode, GreenToken, TextRange, TextSize};
pub use source::{SourceFile, SourceFileId, SourceSpan, TextRangeError};

/// ALLEN's concrete-tree language marker.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum AllenLanguage {}

impl rowan::Language for AllenLanguage {
    type Kind = SyntaxKind;

    fn kind_from_raw(raw: rowan::SyntaxKind) -> Self::Kind {
        SyntaxKind::from_raw(raw.0).expect("rowan syntax kind must be an ALLEN SyntaxKind")
    }

    fn kind_to_raw(kind: Self::Kind) -> rowan::SyntaxKind {
        rowan::SyntaxKind(kind as u16)
    }
}

/// Untyped concrete syntax node for [`AllenLanguage`].
pub type SyntaxNode = rowan::SyntaxNode<AllenLanguage>;
/// Untyped concrete syntax token for [`AllenLanguage`].
pub type SyntaxToken = rowan::SyntaxToken<AllenLanguage>;
/// Untyped concrete syntax element for [`AllenLanguage`].
pub type SyntaxElement = rowan::SyntaxElement<AllenLanguage>;
/// Stable pointer to an immutable concrete syntax node.
pub type SyntaxNodePtr = rowan::ast::SyntaxNodePtr<AllenLanguage>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_tree_facade_supports_typed_wrappers() {
        let mut builder = rowan::GreenNodeBuilder::new();
        builder.start_node(rowan::SyntaxKind(SyntaxKind::Source as u16));
        builder.finish_node();
        let root = SyntaxNode::new_root(builder.finish());
        let source = Source::cast(root).expect("source wrapper");

        assert_eq!(source.syntax().kind(), SyntaxKind::Source);
        assert!(LEXICAL_KIND_INVENTORY.contains(&"trivia:BLOCK_COMMENT"));
    }
}
