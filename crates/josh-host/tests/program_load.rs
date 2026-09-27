mod common;

use base64::Engine as _;
use josh_protocol::{ProgramLoadParams, WireErrorCode};

#[test]
fn verified_bytecode_gets_stable_connection_local_identity() {
    let mut session = common::initialized_session();
    let first = common::load_unit_program(&mut session);
    let second = common::load_unit_program(&mut session);
    assert_eq!(first.program_id, "program-1");
    assert_eq!(second.program_id, "program-2");
    assert_eq!(first.artifact_digest, second.artifact_digest);
    assert_eq!(first.tool_contract_digest, second.tool_contract_digest);
}

#[test]
fn invalid_artifacts_create_no_program_id() {
    let mut session = common::initialized_session();
    let invalid = session
        .load_program(&ProgramLoadParams::Bytecode {
            artifact: "bm90IGFuIGFydGlmYWN0".to_owned(),
        })
        .unwrap_err();
    assert_eq!(invalid.error.code, WireErrorCode::ProgramInvalid);
    assert!(
        invalid.diagnostics.is_empty(),
        "bytecode failures have no source diagnostics"
    );
    assert_eq!(
        common::load_unit_program(&mut session).program_id,
        "program-1"
    );
}

#[test]
fn source_bundle_rejects_undeclared_package_resources() {
    let mut session = common::initialized_session();
    let manifest = r#"[package]
name = "source-test"
version = "0.1.0"
language = ">=0.1.0, <0.2.0"

[[entry]]
name = "main"
function = "src/main.allen::main"
input = "Void"
output = "Void"
"#;
    let error = session
        .load_program(&ProgramLoadParams::SourceBundle {
            files: vec![
                josh_protocol::SourceFile {
                    path: "allen.toml".to_owned(),
                    encoding: josh_protocol::FileEncoding::Utf8,
                    content: manifest.to_owned(),
                },
                josh_protocol::SourceFile {
                    path: "assets/data.bin".to_owned(),
                    encoding: josh_protocol::FileEncoding::Base64,
                    content: "c2VjcmV0".to_owned(),
                },
                josh_protocol::SourceFile {
                    path: "src/main.allen".to_owned(),
                    encoding: josh_protocol::FileEncoding::Utf8,
                    content: "export fn main() returns Void { () }\n".to_owned(),
                },
            ],
        })
        .unwrap_err();
    assert_eq!(error.error.code, WireErrorCode::ProgramInvalid);
}

fn template_bundle(template: Option<&[u8]>, extra: bool) -> ProgramLoadParams {
    let mut files = vec![
        josh_protocol::SourceFile {
            path: "allen.lock".to_owned(),
            encoding: josh_protocol::FileEncoding::Utf8,
            content: include_str!("../../../examples/template-package/allen.lock").to_owned(),
        },
        josh_protocol::SourceFile {
            path: "allen.toml".to_owned(),
            encoding: josh_protocol::FileEncoding::Utf8,
            content: include_str!("../../../examples/template-package/allen.toml").to_owned(),
        },
        josh_protocol::SourceFile {
            path: "src/main.allen".to_owned(),
            encoding: josh_protocol::FileEncoding::Utf8,
            content: include_str!("../../../examples/template-package/src/main.allen").to_owned(),
        },
    ];
    if let Some(template) = template {
        files.push(josh_protocol::SourceFile {
            path: "templates/notice.txt".to_owned(),
            encoding: josh_protocol::FileEncoding::Base64,
            content: base64::engine::general_purpose::STANDARD.encode(template),
        });
    }
    if extra {
        files.push(josh_protocol::SourceFile {
            path: "templates/extra.txt".to_owned(),
            encoding: josh_protocol::FileEncoding::Base64,
            content: base64::engine::general_purpose::STANDARD.encode(b"extra"),
        });
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    ProgramLoadParams::SourceBundle { files }
}

#[test]
fn source_bundle_compiles_exact_declared_template_resources() {
    let mut session = common::initialized_session();
    let loaded = session
        .load_program(&template_bundle(
            Some(include_bytes!(
                "../../../examples/template-package/templates/notice.txt"
            )),
            false,
        ))
        .unwrap();
    assert_eq!(loaded.program_id, "program-1");
    assert_eq!(loaded.entries[0].name, "main");
}

#[test]
fn source_bundle_rejects_missing_tampered_extra_and_oversized_resources() {
    for bundle in [
        template_bundle(None, false),
        template_bundle(
            Some(b"Hello, {{name}}. Count: {{count}}. Enabled: {{enabled}}. changed"),
            false,
        ),
        template_bundle(
            Some(include_bytes!(
                "../../../examples/template-package/templates/notice.txt"
            )),
            true,
        ),
        template_bundle(Some(&vec![b'x'; 1_048_577]), false),
    ] {
        let mut session = common::initialized_session();
        assert_eq!(
            session.load_program(&bundle).unwrap_err().error.code,
            WireErrorCode::ProgramInvalid
        );
    }
}

#[test]
fn one_loose_source_file_gets_a_synthesized_manifest() {
    let mut session = common::initialized_session();
    let loaded = session
        .load_program(&ProgramLoadParams::SourceBundle {
            files: vec![josh_protocol::SourceFile {
                path: "src/main.allen".to_owned(),
                encoding: josh_protocol::FileEncoding::Utf8,
                content: "export fn main() returns Int { 42 }\n".to_owned(),
            }],
        })
        .unwrap();
    assert_eq!(loaded.program_id, "program-1");
    assert_eq!(loaded.entries.len(), 1);
    assert_eq!(loaded.entries[0].name, "main");
}

#[test]
fn source_bundle_loads_control_flow() {
    let mut session = common::initialized_session();
    let loaded = session
        .load_program(&ProgramLoadParams::SourceBundle {
            files: vec![josh_protocol::SourceFile {
                path: "src/main.allen".to_owned(),
                encoding: josh_protocol::FileEncoding::Utf8,
                content: r"fn choose(first: Bool, second: Bool) returns Int {
  if (first) { 1 } else /* comments remain whitespace */ if (second) { 2 } else { 3 }
}

export fn main() returns Void {
  let answer = choose(false, true);
  if (answer == 2) { return; }
  return;
}
"
                .to_owned(),
            }],
        })
        .expect("source bundles accept control flow");

    assert_eq!(loaded.program_id, "program-1");
    assert_eq!(loaded.entries.len(), 1);
    assert_eq!(loaded.entries[0].name, "main");
}

#[test]
fn source_bundle_loads_loops_and_iteration() {
    let mut session = common::initialized_session();
    let loaded = session
        .load_program(&ProgramLoadParams::SourceBundle {
            files: vec![josh_protocol::SourceFile {
                path: "src/main.allen".to_owned(),
                encoding: josh_protocol::FileEncoding::Utf8,
                content: r#"export fn main() returns Int {
  mut total = 0;
  for value in [1, 2, 3] { total = total + value; }
  for (_, value) in map { "z": 3, "a": 4 } { total = total + value; }
  for index in 0..4 {
    if (index == 1) { continue; }
    if (index == 3) { break; }
    total = total + index;
  }
  while (total < 16) { total = total + 1; }
  loop { break; }
  total
}
"#
                .to_owned(),
            }],
        })
        .expect("source bundles accept loops and iteration");

    assert_eq!(loaded.program_id, "program-1");
    assert_eq!(loaded.entries.len(), 1);
    assert_eq!(loaded.entries[0].name, "main");
}

#[test]
fn source_bundle_loads_operators_and_short_circuit_control_flow() {
    let mut session = common::initialized_session();
    let loaded = session
        .load_program(&ProgramLoadParams::SourceBundle {
            files: vec![josh_protocol::SourceFile {
                path: "src/main.allen".to_owned(),
                encoding: josh_protocol::FileEncoding::Utf8,
                content: r"export fn main() returns Int {
  mut value = -13;
  value %= 5;
  value += 10;
  value -= 2;
  value *= 3;
  value /= 2;
  mut decimal: Float = 2.0;
  decimal += 3.0;
  decimal -= 1.5;
  decimal *= 2.0;
  decimal /= 2.0;
  let skipped = false && ((1 % 0) == 0);
  let selected = true || ((1 % 0) == 0);
  if (skipped) { value += 1000; }
  if (selected) { value += 1; }
  value
}
"
                .to_owned(),
            }],
        })
        .expect("source bundles accept operators");

    assert_eq!(loaded.program_id, "program-1");
    assert_eq!(loaded.entries.len(), 1);
    assert_eq!(loaded.entries[0].name, "main");
}

fn loose(source: &str) -> ProgramLoadParams {
    ProgramLoadParams::SourceBundle {
        files: vec![josh_protocol::SourceFile {
            path: "src/main.allen".to_owned(),
            encoding: josh_protocol::FileEncoding::Utf8,
            content: source.to_owned(),
        }],
    }
}

#[test]
fn source_that_does_not_compile_reports_the_same_diagnostics_on_load_check_and_embedders() {
    let source = "export fn main() returns Int {\n  let text = \"🦀\";\r\n  missing + 1\n}\n";
    let mut session = common::initialized_session();
    let checked = session.check_program(&loose(source)).unwrap();
    let loaded = session.load_program(&loose(source)).unwrap_err();
    let catalog =
        allen_schema::FrozenCatalog::freeze(Vec::new(), &allen_schema::CatalogLimits::default())
            .unwrap();
    let embedded = josh_host::compile_source_bundle(
        &[josh_protocol::SourceFile {
            path: "src/main.allen".to_owned(),
            encoding: josh_protocol::FileEncoding::Utf8,
            content: source.to_owned(),
        }],
        &catalog,
        1_048_576,
    )
    .unwrap_err();
    assert_eq!(loaded.error.code, WireErrorCode::ProgramInvalid);
    assert_eq!(loaded.error.message, "source program does not compile");
    assert_eq!(checked.diagnostics, loaded.diagnostics);
    assert_eq!(embedded.diagnostics, loaded.diagnostics);
    assert!(checked.artifact_digest.is_none() && checked.entries.is_empty());
    let [diagnostic] = loaded.diagnostics.as_slice() else {
        panic!("one diagnostic: {:?}", loaded.diagnostics);
    };
    assert_eq!(diagnostic.source, "src/main.allen");
    assert_eq!(
        &source[diagnostic.start as usize..diagnostic.end as usize],
        "missing"
    );
    // The CRLF on line 2 ends one line, as in rendered compiler errors.
    assert_eq!(
        (
            diagnostic.line,
            diagnostic.column,
            diagnostic.end_line,
            diagnostic.end_column
        ),
        (3, 3, 3, 10)
    );
    let wire = loaded.wire();
    let data: josh_protocol::ProgramInvalidData =
        serde_json::from_value(wire.data.clone().unwrap()).unwrap();
    josh_protocol::Validate::validate(&data).unwrap();
    assert_eq!(data.diagnostics, loaded.diagnostics);
    assert_eq!(
        allen_compiler::assemble_inline_source(source, &catalog).unwrap_err(),
        format!(
            "main.allen:3:3: error[{}]: {}",
            diagnostic.code, diagnostic.message
        )
    );
}

#[test]
fn boundary_and_manifest_errors_without_a_span_are_one_diagnostic_at_the_start() {
    let mut session = common::initialized_session();
    for (source, message) in [
        (
            "export fn main() returns fn(Int) returns Int { fn(x: Int) returns Int { x } }\n",
            "manifest.entry: entry 'main' uses a non-serializable boundary type",
        ),
        (
            "manifest { language: \"0.2\" entry: main capabilities: [] }\nexport fn main() returns Int { 1 }\n",
            "inline manifest language must be exactly '0.1'",
        ),
    ] {
        let error = session.load_program(&loose(source)).unwrap_err();
        let [diagnostic] = error.diagnostics.as_slice() else {
            panic!("one diagnostic: {:?}", error.diagnostics);
        };
        assert_eq!(
            (
                diagnostic.code.as_str(),
                diagnostic.message.as_str(),
                diagnostic.source.as_str(),
                diagnostic.line,
                diagnostic.column,
                diagnostic.start,
                diagnostic.end
            ),
            ("E3005", message, "src/main.allen", 1, 1, 0, 0)
        );
    }
}

#[test]
fn check_returns_the_program_contract_without_loading() {
    let mut session = common::initialized_session();
    let source = "record In { name: String }\nexport fn main(input: In) returns Int {\n  mut total = 0;\n  for c in input.name { total += 1; }\n  total\n}\n";
    let checked = session.check_program(&loose(source)).unwrap();
    josh_protocol::Validate::validate(&checked).unwrap();
    assert!(checked.diagnostics.is_empty());
    let [entry] = checked.entries.as_slice() else {
        panic!("one entry");
    };
    assert_eq!(entry.name, "main");
    assert_eq!(
        entry.input_descriptor["properties"]["name"],
        serde_json::json!({"type":"string"})
    );
    assert_eq!(
        entry.output_descriptor,
        serde_json::json!({"type":"integer"})
    );
    let debug = checked.debug.as_ref().unwrap();
    assert_eq!(debug.constructs.len(), 1);
    assert_eq!(debug.constructs[0].line, Some(4));
    // Checking allocates no program ID.
    let loaded = session.load_program(&loose(source)).unwrap();
    assert_eq!(loaded.program_id, "program-1");
    assert_eq!(Some(loaded.artifact_digest), checked.artifact_digest);
    assert_eq!(loaded.entries, checked.entries);
}
