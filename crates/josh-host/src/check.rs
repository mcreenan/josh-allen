//! Compile a source bundle and describe why it does not compile.
//!
//! `program/load`, `program/check`, and embedders that check programs without
//! a connection share [`compile_source_bundle`], so a source that does not
//! compile produces identical diagnostics on every path.

use std::collections::BTreeMap;

use allen_compiler::{
    AssemblyError, CompiledPackage, Diagnostic, SourceLines, assemble_inline_source_checked,
    assemble_root_source_package_with_resources_checked,
};
use allen_package::LoadLimits;
use allen_schema::FrozenCatalog;
use base64::Engine as _;
use josh_protocol::{
    CompilerDiagnostic, DiagnosticLabel, DiagnosticSeverity, FileEncoding, MAX_DIAGNOSTICS,
    ProgramInvalidData, SourceFile, WireError, WireErrorCode,
};

use crate::HostError;

/// Code for a program error that has no source span, such as an invalid
/// manifest, entry, or boundary type.
const PROGRAM_ERROR_CODE: &str = "E3005";
const MAX_DIAGNOSTIC_TEXT: usize = 1_024;

/// Why a program could not be compiled, verified, or loaded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramError {
    pub error: HostError,
    /// Present when a source program does not compile.
    pub diagnostics: Vec<CompilerDiagnostic>,
}

impl ProgramError {
    /// The wire error, with the diagnostics as `data` when there are any.
    #[must_use]
    pub fn wire(&self) -> WireError {
        let mut error = self.error.wire();
        if !self.diagnostics.is_empty() {
            error.data = serde_json::to_value(ProgramInvalidData {
                diagnostics: self.diagnostics.clone(),
            })
            .ok();
        }
        error
    }

    /// Keep the leading diagnostics whose wire form fits in `budget` bytes.
    pub(crate) fn fit(mut self, budget: usize) -> Self {
        let mut used = 0_usize;
        let kept = self
            .diagnostics
            .iter()
            .take_while(|diagnostic| {
                used = used.saturating_add(
                    serde_json::to_vec(diagnostic).map_or(usize::MAX, |bytes| bytes.len() + 1),
                );
                used <= budget
            })
            .count();
        self.diagnostics
            .truncate(kept.max(1).min(self.diagnostics.len()));
        self
    }
}

impl From<HostError> for ProgramError {
    fn from(error: HostError) -> Self {
        Self {
            error,
            diagnostics: Vec::new(),
        }
    }
}

const fn invalid(message: &'static str) -> HostError {
    HostError {
        code: WireErrorCode::ProgramInvalid,
        message,
    }
}

/// Compile one `source_bundle` against a frozen catalog, as `program/load`
/// does.
///
/// A bundle with `allen.toml` is a package; otherwise it must be one loose or
/// inline-manifest `src/main.allen`. `max_bytes` bounds the decoded bundle.
///
/// # Errors
///
/// Returns `program.invalid` for a malformed or oversized bundle, and, with
/// diagnostics, for a source program that does not compile.
pub fn compile_source_bundle(
    files: &[SourceFile],
    catalog: &FrozenCatalog,
    max_bytes: u64,
) -> Result<CompiledPackage, ProgramError> {
    let mut manifest = None;
    let mut lock = None;
    let mut sources = BTreeMap::new();
    let mut resources = BTreeMap::new();
    let mut decoded_bytes = 0_u64;
    for file in files {
        let decoded = match file.encoding {
            FileEncoding::Utf8 => file.content.as_bytes().to_vec(),
            FileEncoding::Base64 => base64::engine::general_purpose::STANDARD
                .decode(&file.content)
                .map_err(|_| invalid("source data is invalid"))?,
        };
        let file_bytes = u64::try_from(decoded.len()).unwrap_or(u64::MAX);
        decoded_bytes = decoded_bytes
            .checked_add(file_bytes)
            .ok_or_else(|| invalid("source program is too large"))?;
        if decoded_bytes > max_bytes {
            return Err(invalid("source program is too large").into());
        }
        match file.path.as_str() {
            "allen.toml" => manifest = Some(file.content.as_str()),
            "allen.lock" => lock = Some(file.content.as_str()),
            path if is_source_path(path) => {
                sources.insert(path.to_owned(), file.content.clone());
            }
            path => {
                resources.insert(path.to_owned(), decoded);
            }
        }
    }
    let mut limits = LoadLimits::default();
    limits.source_bytes = limits.source_bytes.min(max_bytes);
    limits.manifest_bytes = limits.manifest_bytes.min(max_bytes);
    limits.modules = limits.modules.min(files.len());
    limits.filesystem_entries = limits.filesystem_entries.min(files.len());
    limits.path_bytes = limits.path_bytes.min(4_096);
    let (compiled, root) = if let Some(manifest) = manifest {
        (
            assemble_root_source_package_with_resources_checked(
                manifest,
                &sources,
                &resources,
                lock,
                Some(catalog),
                &limits,
            ),
            "allen.toml",
        )
    } else if lock.is_none() && sources.len() == 1 {
        let source = sources
            .get("src/main.allen")
            .ok_or_else(|| invalid("loose source must be named src/main.allen"))?;
        (
            assemble_inline_source_checked(source, catalog),
            "src/main.allen",
        )
    } else {
        return Err(invalid("source manifest is missing").into());
    };
    compiled.map_err(|error| {
        let mut texts = sources;
        if let Some(manifest) = manifest {
            texts.insert("allen.toml".to_owned(), manifest.to_owned());
        }
        ProgramError {
            error: invalid("source program does not compile"),
            diagnostics: compiler_diagnostics(&error, &texts, root),
        }
    })
}

// Source paths are case-sensitive package paths.
#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn is_source_path(path: &str) -> bool {
    path.starts_with("src/") && path.ends_with(".allen")
}

/// Whether a compiler module identity names a bundle path: they are equal, or
/// one ends with the other after a `/` (package identities such as
/// `pkg://name@1.0.0/src/main.allen`, and the loose-source module `main.allen`).
fn same_file(module: &str, path: &str) -> bool {
    let suffix = |long: &str, short: &str| {
        long.strip_suffix(short)
            .is_some_and(|prefix| prefix.ends_with('/'))
    };
    module == path || suffix(module, path) || suffix(path, module)
}

/// Convert an assembly failure into wire diagnostics.
///
/// `texts` maps bundle paths to their text, and a diagnostic names the bundle
/// path of its module (a diagnostic without a module belongs to the compiled
/// root module). A failure without a source span is one diagnostic at the
/// start of `root`.
#[must_use]
pub fn compiler_diagnostics(
    error: &AssemblyError,
    texts: &BTreeMap<String, String>,
    root: &str,
) -> Vec<CompilerDiagnostic> {
    let empty = String::new();
    match error {
        AssemblyError::Diagnostics {
            root: root_module,
            diagnostics,
            ..
        } => {
            let lines = texts
                .iter()
                .map(|(path, text)| (path.as_str(), SourceLines::new(text)))
                .collect::<BTreeMap<_, _>>();
            diagnostics
                .iter()
                .take(MAX_DIAGNOSTICS)
                .map(|diagnostic| {
                    let module = diagnostic.source.as_deref().unwrap_or(root_module);
                    let path = texts
                        .keys()
                        .find(|path| same_file(module, path))
                        .map_or(module, String::as_str);
                    let fallback = SourceLines::new(&empty);
                    let lines = lines.get(path).unwrap_or(&fallback);
                    diagnostic_payload(diagnostic, path, lines)
                })
                .collect()
        }
        AssemblyError::Program(message) => {
            let lines = SourceLines::new(texts.get(root).unwrap_or(&empty));
            let diagnostic = Diagnostic {
                code: PROGRAM_ERROR_CODE,
                message: message.clone(),
                span: allen_compiler::Span { start: 0, end: 0 },
                labels: Vec::new(),
                source: None,
            };
            vec![diagnostic_payload(&diagnostic, root, &lines)]
        }
    }
}

fn diagnostic_payload(
    diagnostic: &Diagnostic,
    path: &str,
    lines: &SourceLines,
) -> CompilerDiagnostic {
    let span = |span: allen_compiler::Span| {
        let (line, column) = lines.position(span.start);
        let (end_line, end_column) = lines.position(span.end.max(span.start));
        (
            u32::try_from(span.start).unwrap_or(u32::MAX),
            u32::try_from(span.end.max(span.start)).unwrap_or(u32::MAX),
            u32::try_from(line).unwrap_or(u32::MAX),
            u32::try_from(column).unwrap_or(u32::MAX),
            u32::try_from(end_line).unwrap_or(u32::MAX),
            u32::try_from(end_column).unwrap_or(u32::MAX),
        )
    };
    let (start, end, line, column, end_line, end_column) = span(diagnostic.span);
    CompilerDiagnostic {
        code: diagnostic.code.to_owned(),
        severity: DiagnosticSeverity::Error,
        message: bounded(&diagnostic.message),
        source: path.to_owned(),
        start,
        end,
        line,
        column,
        end_line,
        end_column,
        labels: diagnostic
            .labels
            .iter()
            .take(16)
            .map(|label| {
                let (start, end, line, column, end_line, end_column) = span(label.span);
                DiagnosticLabel {
                    message: bounded(&label.message),
                    source: path.to_owned(),
                    start,
                    end,
                    line,
                    column,
                    end_line,
                    end_column,
                }
            })
            .collect(),
        notes: Vec::new(),
        help: None,
    }
}

fn bounded(text: &str) -> String {
    let mut end = text.len().min(MAX_DIAGNOSTIC_TEXT);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}
