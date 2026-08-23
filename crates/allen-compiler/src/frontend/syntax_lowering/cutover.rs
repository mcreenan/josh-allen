//! Closed-corpus and source-mode evidence for the canonical production frontend.

use super::*;
use crate::frontend::{PackageEntryPoint, PackageSourceBundle};
use allen_bytecode::{BYTECODE_VERSION, encode};
use allen_package::{LoadLimits, generate_lock, load_verified_package};
use allen_schema::{
    CatalogLimits, FrozenCatalog, Idempotency, SchemaLimits, ToolDefinition, digest_bytes,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

const EXAMPLE_SOURCE_COUNT: usize = 79;
const CLI_FIXTURE_SOURCE_COUNT: usize = 101;
const SYNTAX_GOLDEN_SOURCE_COUNT: usize = 2;
const PARSER_SEED_COUNT: usize = 15;
const EXAMPLE_CORPUS_DIGEST: &str =
    "90fad118ed79083f704d166820fcbd2b8f6eb0658efe10494fd042c8ac3ee113";
const CLI_FIXTURE_CORPUS_DIGEST: &str =
    "cf577e3187ce1cf79812053186315dc5fb8a06d195ad9e4df8dd897ba8e4587e";
const SYNTAX_GOLDEN_CORPUS_DIGEST: &str =
    "57a411cd0f210f99bd3bd3700a5945275c4b2f91d7147a658ac5a8665d7cc28e";
const PARSER_SEED_CORPUS_DIGEST: &str =
    "38b8986f56ada7d3c75af46f4328beec0312c79c0f60b6b2c87380e913976e94";
const COMPLETE_CORPUS_DIGEST: &str =
    "cf132997705aff036493baa83ba894b8fe91f21863f6161b95cb4df8e98d540e";

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

fn tracked_files(root: &Path, predicate: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let repository = repository_root();
    let root = root
        .canonicalize()
        .unwrap_or_else(|error| panic!("canonicalize {}: {error}", root.display()));
    assert!(
        root.starts_with(&repository),
        "tracked root escaped repository"
    );
    let root_relative = root
        .strip_prefix(&repository)
        .expect("repository-contained tracked root")
        .to_str()
        .expect("tracked root path must be UTF-8");
    let output = Command::new("git")
        .args(["ls-files", "-z", "--", root_relative])
        .current_dir(&repository)
        .output()
        .expect("run git ls-files for committed corpus");
    assert!(output.status.success(), "git ls-files failed");
    assert!(
        output.stdout.is_empty() || output.stdout.ends_with(&[0]),
        "git ls-files output is truncated"
    );

    let mut canonical_targets = BTreeSet::new();
    let mut files = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|bytes| !bytes.is_empty())
        .map(|bytes| {
            let relative = std::str::from_utf8(bytes).expect("tracked path must be UTF-8");
            let relative_path = Path::new(relative);
            assert!(
                relative_path
                    .components()
                    .all(|component| matches!(component, Component::Normal(_))),
                "tracked path is not normalized: {relative}"
            );
            let path = repository.join(relative_path);
            let metadata = fs::symlink_metadata(&path)
                .unwrap_or_else(|error| panic!("metadata {}: {error}", path.display()));
            assert!(!metadata.file_type().is_symlink(), "tracked corpus symlink");
            assert!(metadata.is_file(), "tracked corpus entry is not a file");
            let canonical = path.canonicalize().expect("canonical tracked corpus path");
            assert!(canonical.starts_with(&root), "tracked path escaped root");
            assert!(
                canonical_targets.insert(canonical.clone()),
                "duplicate target"
            );
            canonical
        })
        .filter(|path| predicate(path))
        .collect::<Vec<_>>();
    files.sort_by_key(|path| relative(path));
    files
}

fn allen_sources(root: &Path) -> Vec<PathBuf> {
    tracked_files(root, |path| {
        path.extension()
            .is_some_and(|extension| extension == "allen")
    })
}

fn relative(path: &Path) -> String {
    path.strip_prefix(repository_root())
        .expect("repository path")
        .to_str()
        .expect("committed path must be UTF-8")
        .replace('\\', "/")
}

fn read_source(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn corpus_digest(paths: &[PathBuf]) -> String {
    use std::fmt::Write as _;

    let mut inventory = String::new();
    let mut sorted = paths.iter().collect::<Vec<_>>();
    sorted.sort_by_key(|path| relative(path));
    for path in sorted {
        let bytes = fs::read(path).expect("read corpus file");
        writeln!(
            inventory,
            "{}  {}",
            digest_bytes(&bytes)
                .strip_prefix("sha256:")
                .expect("canonical digest prefix"),
            relative(path)
        )
        .expect("write corpus inventory");
    }
    digest_bytes(inventory.as_bytes())
        .strip_prefix("sha256:")
        .expect("canonical digest prefix")
        .to_owned()
}

fn corpus_classes() -> (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>) {
    let root = repository_root();
    (
        allen_sources(&root.join("examples")),
        allen_sources(&root.join("crates/allen-cli/tests/fixtures")),
        allen_sources(&root.join("crates/allen-syntax/test-data")),
        tracked_files(&root.join("fuzz/seeds/parser"), |path| {
            path.file_name().is_some_and(|name| name != "README.md")
        }),
    )
}

fn empty_catalog() -> FrozenCatalog {
    FrozenCatalog::freeze(Vec::new(), &CatalogLimits::default()).expect("empty catalog")
}

fn echo_catalog() -> FrozenCatalog {
    let definition = ToolDefinition::parse(
        "demo.echo",
        "1.2.0",
        r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}"#,
        r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}"#,
        r#"{"type":"object","properties":{"code":{"type":"string"},"message":{"type":"string"}},"required":["code","message"],"additionalProperties":false}"#,
        vec!["external.write".to_owned()],
        Idempotency::Idempotent,
        &SchemaLimits::default(),
    )
    .expect("echo definition");
    FrozenCatalog::freeze(vec![definition], &CatalogLimits::default()).expect("echo catalog")
}

#[derive(Debug)]
struct PreparedCatalogRoute {
    compilation: crate::Compilation,
    prepared: crate::PreparedTools,
    package: crate::CompiledPackage,
    encoded_artifact: Vec<u8>,
}

fn compile_prepared_catalog_route(source: &str, support: &str) -> PreparedCatalogRoute {
    let checked = lower_source("main.allen", source).expect("catalog example syntax");
    let manifest = checked.manifest.expect("catalog example manifest");
    let mut prepared =
        crate::prepare_tools(&echo_catalog(), &manifest.tools).expect("prepare fresh tools");
    let bundle = PackageSourceBundle {
        root: "main.allen".to_owned(),
        sources: BTreeMap::from([
            ("main.allen".to_owned(), source.to_owned()),
            (
                "functions-and-effects/support.allen".to_owned(),
                support.to_owned(),
            ),
        ]),
        import_targets: BTreeMap::new(),
        entry_points: vec![PackageEntryPoint {
            module: "main.allen".to_owned(),
            function: "main".to_owned(),
        }],
        entry_modules: vec!["functions-and-effects/support.allen".to_owned()],
    };
    let compilation = crate::compile_package_bundle_with_prepared_tools(&bundle, &mut prepared)
        .expect("catalog route compiles");
    let package =
        crate::assemble_inline_compilation(manifest, compilation.clone(), prepared.clone())
            .expect("catalog route assembles");
    let encoded_artifact = encode(&package.artifact).expect("catalog artifact encodes");
    PreparedCatalogRoute {
        compilation,
        prepared,
        package,
        encoded_artifact,
    }
}

#[test]
fn closed_197_input_inventory_is_complete_disjoint_and_unchanged() {
    let (examples, fixtures, goldens, seeds) = corpus_classes();
    assert_eq!(examples.len(), EXAMPLE_SOURCE_COUNT);
    assert_eq!(fixtures.len(), CLI_FIXTURE_SOURCE_COUNT);
    assert_eq!(goldens.len(), SYNTAX_GOLDEN_SOURCE_COUNT);
    assert_eq!(seeds.len(), PARSER_SEED_COUNT);
    assert_eq!(corpus_digest(&examples), EXAMPLE_CORPUS_DIGEST);
    assert_eq!(corpus_digest(&fixtures), CLI_FIXTURE_CORPUS_DIGEST);
    assert_eq!(corpus_digest(&goldens), SYNTAX_GOLDEN_CORPUS_DIGEST);
    assert_eq!(corpus_digest(&seeds), PARSER_SEED_CORPUS_DIGEST);

    let complete = examples
        .iter()
        .chain(&fixtures)
        .chain(&goldens)
        .chain(&seeds)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(complete.len(), 197);
    assert_eq!(
        complete
            .iter()
            .map(|path| relative(path))
            .collect::<BTreeSet<_>>()
            .len(),
        complete.len()
    );
    assert_eq!(corpus_digest(&complete), COMPLETE_CORPUS_DIGEST);
}

#[test]
fn every_utf8_corpus_input_has_a_deterministic_lossless_canonical_outcome() {
    let (examples, fixtures, goldens, seeds) = corpus_classes();
    let complete = examples
        .into_iter()
        .chain(fixtures)
        .chain(goldens)
        .chain(seeds)
        .collect::<Vec<_>>();
    let mut utf8 = 0usize;
    let mut invalid_utf8 = 0usize;
    for path in complete {
        let bytes = fs::read(&path).expect("read corpus input");
        let Ok(text) = std::str::from_utf8(&bytes) else {
            let error = std::str::from_utf8(&bytes).expect_err("invalid UTF-8 seed");
            assert!(relative(&path).ends_with("invalid-utf8"));
            assert_eq!((error.valid_up_to(), error.error_len()), (26, Some(1)));
            invalid_utf8 += 1;
            continue;
        };
        utf8 += 1;
        let source = SourceFile::new(SourceFileId::new(7), text).expect("bounded corpus source");
        let first = allen_syntax::parse(&source);
        let second = allen_syntax::parse(&source);
        assert_eq!(
            first.diagnostics(),
            second.diagnostics(),
            "{}",
            relative(&path)
        );
        assert_eq!(
            format!("{:#?}", first.syntax()),
            format!("{:#?}", second.syntax())
        );
        assert!(tree_matches_source(&first.syntax(), text));
        if !first.has_errors() {
            let first_lowered = lower_checked(&source, &first);
            let second_lowered = lower_checked(&source, &second);
            assert_eq!(
                format!("{first_lowered:#?}"),
                format!("{second_lowered:#?}")
            );
        }
    }
    assert_eq!((utf8, invalid_utf8), (196, 1));
}

#[test]
fn every_production_source_mode_is_deterministic_on_the_closed_routes() {
    let root = repository_root();
    let excluded = BTreeSet::from(["examples/learnxinyminutes.allen"]);
    let mut loose = 0usize;
    for path in allen_sources(&root.join("examples")) {
        let label = relative(&path);
        if label.contains("/filesystem-package/")
            || label.contains("/functions-and-effects/")
            || excluded.contains(label.as_str())
        {
            continue;
        }
        let source = read_source(&path);
        if crate::extract_inline_manifest(&source)
            .expect("example manifest parses")
            .0
            .is_some()
        {
            continue;
        }
        let first = crate::compile_source(&source).expect("loose example compiles");
        let second = crate::compile_source(&source).expect("loose example recompiles");
        assert_eq!(first, second, "{label}");
        let first = crate::assemble_loose_compilation("main.allen", first).expect("loose artifact");
        let second =
            crate::assemble_loose_compilation("main.allen", second).expect("loose artifact");
        assert_eq!(first, second, "{label}");
        assert_eq!(first.artifact.metadata.bytecode_version, BYTECODE_VERSION);
        assert_eq!(
            encode(&first.artifact).unwrap(),
            encode(&second.artifact).unwrap()
        );
        loose += 1;
    }
    assert_eq!(loose, 53);

    let module_root = root.join("examples/functions-and-effects");
    let module_sources = BTreeMap::from([
        (
            "main.allen".to_owned(),
            read_source(&module_root.join("main.allen")),
        ),
        (
            "support.allen".to_owned(),
            read_source(&module_root.join("support.allen")),
        ),
    ]);
    assert_eq!(
        crate::compile_bundle("main.allen", &module_sources),
        crate::compile_bundle("main.allen", &module_sources)
    );

    let catalog = empty_catalog();
    let inline_routes = [
        "examples/filesystem-inline.allen",
        "examples/josh-answer.allen",
        "crates/allen-cli/tests/fixtures/comments/parity/inline-clean.allen",
        "crates/allen-cli/tests/fixtures/control-flow/parity/inline.allen",
        "crates/allen-cli/tests/fixtures/frontend/parity/inline.allen",
        "crates/allen-cli/tests/fixtures/loops/parity/inline.allen",
        "crates/allen-cli/tests/fixtures/operators/parity/inline.allen",
        "crates/allen-cli/tests/fixtures/strings/parity/inline.allen",
    ];
    for route in inline_routes {
        let source = read_source(&root.join(route));
        assert_eq!(
            crate::assemble_inline_source(&source, &catalog),
            crate::assemble_inline_source(&source, &catalog),
            "{route}"
        );
    }
}

#[test]
fn package_and_module_routes_are_canonical() {
    let root = repository_root();
    let fixture_root = root.join("crates/allen-cli/tests/fixtures");
    let fixture_sources = allen_sources(&fixture_root);
    let tracked_sources = fixture_sources.iter().cloned().collect::<BTreeSet<_>>();
    let mut module_directories = fixture_sources
        .iter()
        .filter(|path| {
            path.file_name().is_some_and(|name| name == "main.allen")
                && path.parent().is_some_and(|directory| {
                    tracked_sources.contains(&directory.join("support.allen"))
                })
        })
        .map(|path| path.parent().expect("main parent").to_owned())
        .collect::<Vec<_>>();
    module_directories.sort();
    module_directories.dedup();
    assert_eq!(module_directories.len(), 10);
    for directory in module_directories {
        let sources = BTreeMap::from([
            (
                "main.allen".to_owned(),
                read_source(&directory.join("main.allen")),
            ),
            (
                "support.allen".to_owned(),
                read_source(&directory.join("support.allen")),
            ),
        ]);
        assert_eq!(
            crate::compile_bundle("main.allen", &sources),
            crate::compile_bundle("main.allen", &sources)
        );
    }

    let mut manifests = tracked_files(&root.join("examples"), |path| {
        path.file_name().is_some_and(|name| name == "allen.toml")
    });
    manifests.extend(tracked_files(&fixture_root, |path| {
        path.file_name().is_some_and(|name| name == "allen.toml")
    }));
    manifests.sort();
    let tracked_manifests = manifests.iter().cloned().collect::<BTreeSet<_>>();
    let package_roots = manifests
        .into_iter()
        .filter_map(|manifest| {
            let directory = manifest.parent().expect("manifest parent");
            let nested = directory
                .ancestors()
                .skip(1)
                .take_while(|ancestor| ancestor.starts_with(&root))
                .any(|ancestor| tracked_manifests.contains(&ancestor.join("allen.toml")));
            (!nested).then(|| directory.to_owned())
        })
        .collect::<Vec<_>>();
    assert_eq!(package_roots.len(), 9);
    for package_root in package_roots {
        let limits = LoadLimits::default();
        let lock = generate_lock(&package_root, &limits).expect("generate package lock");
        let loaded = load_verified_package(&package_root, &lock, &limits).expect("load package");
        assert_eq!(
            crate::assemble_loaded_package(&loaded, None),
            crate::assemble_loaded_package(&loaded, None)
        );
    }
}

#[test]
fn prepared_catalog_route_is_repeat_deterministic_from_fresh_state() {
    let root = repository_root();
    let source = read_source(&root.join("examples/learnxinyminutes.allen"));
    let support = read_source(&root.join("examples/functions-and-effects/support.allen"));
    let first = compile_prepared_catalog_route(&source, &support);
    let second = compile_prepared_catalog_route(&source, &support);

    assert_eq!(first.compilation, second.compilation);
    assert_eq!(first.prepared, second.prepared);
    assert_eq!(first.package, second.package);
    assert_eq!(
        first.package.artifact.metadata.bytecode_version,
        BYTECODE_VERSION
    );
    assert_eq!(
        second.package.artifact.metadata.bytecode_version,
        BYTECODE_VERSION
    );
    assert_eq!(first.encoded_artifact, second.encoded_artifact);
}

#[test]
fn authoritative_early_alpha_syntax_consequences_are_source_qualified() {
    for source in [
        "export\u{000c}fn main() returns Void { () }",
        "import {} from \"./dep.allen\"; export fn main() returns Void { () }",
        "export fn main(value: (Int)) returns Void { () }",
        "export fn main() returns Void { let value = return; () }",
    ] {
        let diagnostic = &crate::compile_source(source).expect_err("obsolete syntax rejected")[0];
        assert_eq!(diagnostic.code, "E3005");
        assert_eq!(diagnostic.source.as_deref(), Some("main.allen"));
        assert!(diagnostic.message.contains("(S0"));
    }

    let versioned = r#"manifest { language: "allen-0.1", entry: main, capabilities: [tool.demo.echo@1] }
export fn main() returns Void { () }
"#;
    let checked = lower_source("versioned-inline.allen", versioned)
        .expect("versioned capability is current syntax");
    assert_eq!(
        checked.manifest.expect("inline manifest").capabilities,
        ["tool.demo.echo@1"]
    );
}

#[test]
fn production_sources_reference_only_the_canonical_frontend() {
    let root = repository_root();
    let sources = [
        root.join("crates/allen-compiler/src/frontend.rs"),
        root.join("crates/allen-compiler/src/package.rs"),
        root.join("crates/allen-compiler/src/frontend/syntax_lowering.rs"),
        root.join("crates/allen-compiler/src/frontend/syntax_lowering/expressions.rs"),
    ];
    let forbidden = [
        ["parse_", "leg", "acy_module"].concat(),
        ["Module", "Parser"].concat(),
        ["Ast", "Module"].concat(),
        ["Ast", "Expr"].concat(),
        ["Ast", "Type"].concat(),
        ["with_import_targets", "_and_parser"].concat(),
        ["assemble_loaded_package", "_syntax"].concat(),
        ["compile_inline_manifest_source_with_catalog", "_syntax"].concat(),
    ];
    for path in sources {
        let text = read_source(&path);
        for symbol in &forbidden {
            assert!(
                !text.contains(symbol),
                "{} contains {symbol}",
                relative(&path)
            );
        }
    }
}

#[test]
fn cli_reuses_one_prepared_root_across_inline_and_loose_branches() {
    let path = repository_root().join("crates/allen-cli/src/app.rs");
    let text = read_source(&path);
    let start = text
        .find("fn compile_source_artifact(")
        .expect("CLI source compiler exists");
    let end = text[start..]
        .find("\nfn format_effect_report_entry(")
        .map(|offset| start + offset)
        .expect("CLI source compiler boundary exists");
    let body = &text[start..end];
    assert_eq!(body.matches("prepare_source(").count(), 1);
    assert_eq!(
        body.matches("compile_prepared_inline_manifest_source(")
            .count(),
        1
    );
    assert_eq!(
        body.matches("compile_bundle_with_prepared_source(").count(),
        1
    );
    assert!(!body.contains("extract_inline_manifest("));
    assert!(!body.contains("compile_inline_manifest_source("));
}
