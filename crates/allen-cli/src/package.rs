use allen_bytecode::Artifact;
use allen_compiler::{Compilation, CompiledPackage, InlineManifest};
use allen_package::{LoadLimits, LoadedPackage, load_verified_package};
use allen_schema::{CatalogLimits, FrozenCatalog};
use std::fs;
use std::io::Read;
use std::path::Path;

#[allow(clippy::too_many_lines)]
pub(crate) fn from_inline(
    manifest: InlineManifest,
    compilation: Compilation,
) -> Result<Artifact, String> {
    if !manifest.tools.is_empty() {
        return Err(
            "standalone compilation cannot resolve required tools without a frozen catalog"
                .to_owned(),
        );
    }
    let catalog = FrozenCatalog::freeze(Vec::new(), &CatalogLimits::default())
        .map_err(|error| error.to_string())?;
    let prepared =
        allen_compiler::prepare_tools(&catalog, &[]).map_err(|error| error.to_string())?;
    allen_compiler::assemble_inline_compilation(manifest, compilation, prepared)
        .map(|compiled| compiled.artifact)
}

pub(crate) fn load_and_compile(root: &Path) -> Result<CompiledPackage, String> {
    let lock_path = root.join("allen.lock");
    let lock_text = read_bounded_utf8(&lock_path, 16 * 1024 * 1024)?;
    let loaded = load_verified_package(root, &lock_text, &LoadLimits::default())
        .map_err(|error| error.to_string())?;
    compile_loaded(&loaded)
}

pub(crate) fn load_test_package(root: &Path) -> Result<LoadedPackage, String> {
    let lock_path = root.join("allen.lock");
    let lock_text = read_bounded_utf8(&lock_path, 16 * 1024 * 1024)?;
    load_verified_package(root, &lock_text, &LoadLimits::default())
        .map_err(|error| error.to_string())
}

fn read_bounded_utf8(path: &Path, maximum: usize) -> Result<String, String> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|error| format!("{}: cannot open required lockfile: {error}", path.display()))?
        .take(u64::try_from(maximum).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("{}: cannot read required lockfile: {error}", path.display()))?;
    if bytes.len() > maximum {
        return Err(format!(
            "{}: required lockfile exceeds its byte limit",
            path.display()
        ));
    }
    String::from_utf8(bytes)
        .map_err(|_| format!("{}: required lockfile is not UTF-8", path.display()))
}

fn compile_loaded(loaded: &LoadedPackage) -> Result<CompiledPackage, String> {
    allen_compiler::assemble_loaded_package(loaded, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use allen_bytecode::compute_tool_contract_digest;
    use allen_schema::FrozenCatalog;

    fn catalog_with_lookup() -> FrozenCatalog {
        let definition = allen_schema::ToolDefinition::parse(
            "example.lookup",
            "1.2.3",
            r#"{"type":"boolean"}"#,
            r#"{"type":"string"}"#,
            r#"{"type":"string","enum":["missing"]}"#,
            vec!["external.read".to_owned()],
            allen_schema::Idempotency::Idempotent,
            &allen_schema::SchemaLimits::default(),
        )
        .unwrap();
        FrozenCatalog::freeze(vec![definition], &allen_schema::CatalogLimits::default()).unwrap()
    }

    #[test]
    fn reversed_verified_graph_produces_identical_artifact_bytes() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/filesystem-package");
        let lock = read_bounded_utf8(&root.join("allen.lock"), 16 * 1024 * 1024).unwrap();
        let loaded = load_verified_package(&root, &lock, &LoadLimits::default()).unwrap();
        let forward = compile_loaded(&loaded).unwrap().artifact;
        let mut reversed = loaded;
        reversed.packages.reverse();
        reversed.modules.reverse();
        for package in &mut reversed.packages {
            package.modules.reverse();
            package.dependencies.reverse();
        }
        let reverse = compile_loaded(&reversed).unwrap().artifact;
        assert_eq!(
            allen_bytecode::encode(&forward).unwrap(),
            allen_bytecode::encode(&reverse).unwrap()
        );
    }

    #[test]
    fn package_authority_is_preserved_in_a_verified_contract() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/filesystem-package");
        let lock = read_bounded_utf8(&root.join("allen.lock"), 16 * 1024 * 1024).unwrap();
        let mut loaded = load_verified_package(&root, &lock, &LoadLimits::default()).unwrap();
        let root_id = loaded.root.clone();
        let root_package = loaded
            .packages
            .iter_mut()
            .find(|package| package.id == root_id)
            .expect("root package exists");
        root_package
            .manifest
            .capabilities
            .optional
            .push("net.http_get".to_owned());
        root_package.manifest.capabilities.optional.sort();
        root_package.manifest.network.http_get.origins =
            vec!["https://api.example.test".to_owned()];

        let artifact = compile_loaded(&loaded).expect("package compiles").artifact;
        assert_eq!(
            artifact.metadata.bytecode_version,
            allen_bytecode::BYTECODE_VERSION
        );
        assert_eq!(
            artifact
                .manifest
                .as_ref()
                .expect("manifest contract exists")
                .https_origins,
            ["https://api.example.test".to_owned()]
        );
        let bytes = allen_bytecode::encode(&artifact).expect("package encodes");
        allen_bytecode::decode_and_verify(&bytes, &allen_bytecode::DecodeLimits::default())
            .expect("package verifies");
    }

    #[test]
    fn frozen_catalog_builds_a_verified_tool_contract() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/filesystem-package");
        let lock = read_bounded_utf8(&root.join("allen.lock"), 16 * 1024 * 1024).unwrap();
        let mut loaded = load_verified_package(&root, &lock, &LoadLimits::default()).unwrap();
        let root_id = loaded.root.clone();
        loaded
            .packages
            .iter_mut()
            .find(|package| package.id == root_id)
            .unwrap()
            .manifest
            .tools
            .required
            .push(allen_package::ToolRequirement {
                name: "example.lookup".to_owned(),
                version: ">=1.0.0, <2.0.0".to_owned(),
            });
        let artifact =
            allen_compiler::assemble_loaded_package(&loaded, Some(&catalog_with_lookup()))
                .unwrap()
                .artifact;
        let manifest = artifact.manifest.as_ref().unwrap();
        assert_eq!(manifest.required_tools.len(), 1);
        assert_eq!(manifest.required_tools[0].name, "example.lookup");
        assert_eq!(manifest.required_tools[0].effect, "tool.example.lookup@1");
        assert_eq!(
            manifest.tool_contract_digest,
            compute_tool_contract_digest(&manifest.required_tools)
        );
        let bytes = allen_bytecode::encode(&artifact).unwrap();
        allen_bytecode::decode_and_verify(&bytes, &allen_bytecode::DecodeLimits::default())
            .unwrap();
    }

    #[test]
    fn inline_nominal_enum_uses_a_verified_package_identity() {
        let source = r#"
manifest {
  language: "0.1"
  entry: main
  capabilities: []
}
export enum State { Ready }
export fn main() returns State { State.Ready }
"#;
        let (manifest, compilation) = allen_compiler::compile_inline_manifest_source(source)
            .expect("inline enum source compiles");
        let artifact = from_inline(manifest.expect("inline manifest exists"), compilation)
            .expect("inline artifact is valid");
        let bytes = allen_bytecode::encode(&artifact).expect("inline artifact encodes");
        let verified =
            allen_bytecode::decode_and_verify(&bytes, &allen_bytecode::DecodeLimits::default())
                .expect("inline enum artifact verifies");
        assert_eq!(
            verified.verified_module().module().enum_types[0].name,
            "pkg://inline@0.1.0/src/main.allen::State"
        );
    }

    #[test]
    fn inline_typed_prompt_embeds_its_response_schema() {
        let source = r#"
manifest {
  language: "0.1"
  entry: main
  capabilities: [model.request]
}
record Review { approved: Bool }
export async fn main() returns Void effects [model.request] {
  let review = await model.request(prompt {
    system: "Review the change."
    output: Review
  });
  ()
}
"#;
        let (manifest, compilation) = allen_compiler::compile_inline_manifest_source(source)
            .expect("inline typed prompt compiles");
        let artifact = from_inline(manifest.expect("inline manifest exists"), compilation)
            .expect("inline typed prompt artifact is valid");
        let expected = allen_bytecode::ValueType::Record(vec![allen_bytecode::RecordField {
            name: "approved".to_owned(),
            value_type: allen_bytecode::ValueType::Bool,
        }]);
        assert!(
            artifact
                .schemas
                .iter()
                .any(|schema| schema.value_type == expected)
        );
        let bytes = allen_bytecode::encode(&artifact).expect("typed prompt artifact encodes");
        allen_bytecode::decode_and_verify(&bytes, &allen_bytecode::DecodeLimits::default())
            .expect("typed prompt artifact verifies");
    }

    #[test]
    fn inline_manifest_produces_a_verified_artifact() {
        let source = r#"
manifest {
  language: "0.1"
  entry: main
  capabilities: [net.http_get, permission.request_external_fs]
  http_origins: ["https://api.example.test"]
}
export async fn main() returns Void
  effects [net.http_get, permission.request_external_fs] {
  let response = await http.get("https://api.example.test/status");
  let grant = await permission.request_file({
    access: ExternalFsAccess.Read,
    path: "/outside/notes.txt",
    reason: "Read the selected notes."
  });
  ()
}
"#;
        let (manifest, compilation) =
            allen_compiler::compile_inline_manifest_source(source).expect("inline source compiles");
        let artifact = from_inline(manifest.expect("inline manifest exists"), compilation)
            .expect("inline artifact is valid");
        assert_eq!(
            artifact.metadata.bytecode_version,
            allen_bytecode::BYTECODE_VERSION
        );
        let contract = artifact
            .manifest
            .as_ref()
            .expect("manifest contract exists");
        assert_eq!(
            contract.required_capabilities,
            [
                "net.http_get".to_owned(),
                "permission.request_external_fs".to_owned()
            ]
        );
        assert_eq!(
            contract.https_origins,
            ["https://api.example.test".to_owned()]
        );
        let bytes = allen_bytecode::encode(&artifact).expect("inline artifact encodes");
        allen_bytecode::decode_and_verify(&bytes, &allen_bytecode::DecodeLimits::default())
            .expect("inline artifact verifies");
    }

    #[test]
    fn inline_http_capability_and_origins_must_be_canonical_and_paired() {
        for (capabilities, origins) in [
            ("[net.http_get]", "[]"),
            ("[]", "[\"https://api.example.test\"]"),
            ("[net.http_get]", "[\"https://api.example.test/\"]"),
        ] {
            let source = format!(
                r#"
manifest {{
  language: "0.1"
  entry: main
  capabilities: {capabilities}
  http_origins: {origins}
}}
export fn main() returns Void {{ () }}
"#
            );
            let (manifest, compilation) = allen_compiler::compile_inline_manifest_source(&source)
                .expect("inline manifest syntax is valid");
            assert!(
                from_inline(manifest.expect("inline manifest exists"), compilation).is_err(),
                "invalid HTTP authority pair must fail"
            );
        }
    }
}
