#![forbid(unsafe_code)]

use allen_bytecode::Module;
use std::fmt;

mod frontend;
mod package;

/// Complete stable set of public source diagnostic codes emitted by this compiler.
pub const DIAGNOSTIC_CODES: &[&str] = &[
    "E0002", "E0003", "E0004", "E0005", "E2003", "E2009", "E2011", "E2012", "E2015", "E2016",
    "E2017", "E2018", "E2019", "E2020", "E2403", "E3002", "E3003", "E3005", "E3007", "E3008",
    "E3010", "E3011", "E3012", "E3013",
];

pub use frontend::{
    Compilation, CompiledSourceTest, CompilerTemplateBinding, CompilerToolBinding,
    EffectReportEntry, ExportedFunction, HirBundle, HirConstant, HirExpr, HirExprKind, HirFunction,
    HirModule, InlineManifest, MirBlock, MirBundle, MirCleanupKind, MirConstant, MirFunction,
    MirOperation, MirOwnership, MirOwnershipState, MirSuspension, MirTaskScope, MirTerminator,
    PackageEntryPoint, PackageSourceBundle, PreparedSource, PreparedTools, SourceTest,
    ToolPreparationError, compile_bundle, compile_bundle_with_prepared_source,
    compile_inline_manifest_source, compile_inline_manifest_source_with_catalog,
    compile_package_bundle, compile_package_bundle_with_prepared_tools,
    compile_package_bundle_with_prepared_tools_and_templates, compile_package_bundle_with_tools,
    compile_prepared_inline_manifest_source, compile_source, compile_source_test,
    compile_source_test_with_prepared_tools_and_templates, discover_source_tests,
    extract_inline_manifest, prepare_source, prepare_tools, reachable_source_modules,
};
pub use package::{
    CompiledPackage, CompiledPackageSourceTest, assemble_inline_compilation,
    assemble_inline_source, assemble_loaded_package, assemble_loaded_source_test,
    assemble_loose_compilation, assemble_root_source_package,
    assemble_root_source_package_with_resources, assemble_source_test, prepare_loaded_source_tests,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    pub code: &'static str,
    pub message: String,
    pub span: Span,
    pub labels: Vec<DiagnosticLabel>,
    pub source: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticLabel {
    pub span: Span,
    pub message: String,
}

impl Diagnostic {
    fn new(code: &'static str, message: impl Into<String>, span: Span) -> Self {
        Self {
            code,
            message: message.into(),
            span,
            labels: Vec::new(),
            source: None,
        }
    }

    fn with_label(mut self, span: Span, message: impl Into<String>) -> Self {
        self.labels.push(DiagnosticLabel {
            span,
            message: message.into(),
        });
        self
    }

    fn with_source(mut self, source: impl Into<String>) -> Self {
        if self.source.is_none() {
            self.source = Some(source.into());
        }
        self
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

/// Compile one version 0.1 source file through the canonical frontend.
///
/// The caller must pass the returned module to `allen_bytecode::verify` before
/// execution.
///
/// # Errors
///
/// Returns deterministic lexical, parse, resolution, type, effect, ownership,
/// or lowering diagnostics from the canonical frontend.
pub fn compile(source: &str) -> Result<Module, Vec<Diagnostic>> {
    compile_source(source).map(|compilation| compilation.module)
}

#[must_use]
pub fn render_diagnostic(path: &str, source: &str, diagnostic: &Diagnostic) -> String {
    let offset = diagnostic.span.start.min(source.len());
    let prefix = &source[..offset];
    let bytes = prefix.as_bytes();
    let mut line = 1;
    let mut line_start = 0;
    let mut position = 0;
    while position < bytes.len() {
        match bytes[position] {
            b'\r' => {
                position += 1;
                if bytes.get(position) == Some(&b'\n') {
                    position += 1;
                }
                line += 1;
                line_start = position;
            }
            b'\n' => {
                position += 1;
                line += 1;
                line_start = position;
            }
            _ => position += 1,
        }
    }
    let column = source[line_start..offset].chars().count() + 1;
    format!(
        "{path}:{line}:{column}: error[{}]: {}",
        diagnostic.code, diagnostic.message
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_rendering_counts_every_source_line_terminator_once() {
        for terminator in ["\n", "\r\n", "\r"] {
            let source = format!("// 🦀{terminator}/*");
            let start = source.len() - 2;
            let diagnostic = Diagnostic::new(
                "E0005",
                "unterminated block comment",
                Span {
                    start,
                    end: start + 2,
                },
            );
            assert_eq!(
                render_diagnostic("main.allen", &source, &diagnostic),
                "main.allen:2:1: error[E0005]: unterminated block comment"
            );
        }
    }
}
