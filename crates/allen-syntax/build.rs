mod build_support;

use std::{env, fs, path::PathBuf};

const GRAMMAR_FILE: &str = "allen-0.1.ungram";
const PRODUCTION_MAP_FILE: &str = "production-map.tsv";
const KIND_IDS_FILE: &str = "kind-ids.tsv";
const LEXICAL_KINDS_FILE: &str = "lexical-kinds.tsv";
const GENERATED_FILES: [&str; 3] = ["kinds.rs", "ast.rs", "inventory.rs"];
const REGENERATE_HINT: &str = "run ALLEN_SYNTAX_REGENERATE=1 cargo check -p allen-syntax";

fn main() {
    let crate_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"));
    let grammar_dir = crate_dir.join("grammar");
    let grammar_path = grammar_dir.join(GRAMMAR_FILE);
    let production_map_path = grammar_dir.join(PRODUCTION_MAP_FILE);
    let kind_ids_path = grammar_dir.join(KIND_IDS_FILE);
    let lexical_kinds_path = grammar_dir.join(LEXICAL_KINDS_FILE);
    let language_spec_path = crate_dir.join("../../docs/language-spec.md");

    let grammar = fs::read_to_string(&grammar_path).expect("read grammar");
    let production_map = fs::read_to_string(&production_map_path).expect("read production mapping");
    let kind_ids = fs::read_to_string(&kind_ids_path).expect("read stable kind IDs");
    let lexical_kinds =
        fs::read_to_string(&lexical_kinds_path).expect("read lexical kind inventory");
    let language_spec =
        fs::read_to_string(&language_spec_path).expect("read language specification");

    let inputs = build_support::Inputs {
        grammar: &grammar,
        production_map: &production_map,
        kind_ids: &kind_ids,
        lexical_kinds: &lexical_kinds,
        language_spec: &language_spec,
    };
    let generated = build_support::render(&inputs)
        .unwrap_or_else(|error| panic!("grammar generation failed: {error}"));
    let output_dir = crate_dir.join("src/generated");
    fs::create_dir_all(&output_dir).expect("create generated output directory");
    let regenerate = env::var_os("ALLEN_SYNTAX_REGENERATE").is_some_and(|value| value == "1");

    for (name, bytes) in generated.files() {
        let path = output_dir.join(name);
        if regenerate {
            fs::write(&path, bytes)
                .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
        } else {
            let checked_in = fs::read(&path).unwrap_or_else(|error| {
                panic!("read {}: {error}; {REGENERATE_HINT}", path.display())
            });
            assert_eq!(
                checked_in,
                bytes,
                "{} is stale; {REGENERATE_HINT}",
                path.display()
            );
        }
    }

    for path in [
        grammar_path,
        production_map_path,
        kind_ids_path,
        lexical_kinds_path,
        language_spec_path,
        crate_dir.join("build_support.rs"),
    ] {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    for name in GENERATED_FILES {
        println!("cargo:rerun-if-changed={}", output_dir.join(name).display());
    }
    println!("cargo:rerun-if-env-changed=ALLEN_SYNTAX_REGENERATE");
}
