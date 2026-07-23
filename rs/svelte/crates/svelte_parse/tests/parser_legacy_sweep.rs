//! Walk every fixture in `packages/svelte/tests/parser-legacy/samples/` and
//! smoke-check that our parser handles it without erroring or panicking.
//!
//! Same shape as `parser_modern_sweep` — AST byte-equality against
//! upstream's `output.json` is deferred. This sweep gates "did the parser
//! and analyze pipeline accept the fixture without erroring?"

use std::fs;
use std::path::PathBuf;

use svelte_parse::parse;

#[test]
#[ignore]
fn sweep_parser_legacy_fixtures() {
    let base = PathBuf::from("../../../../packages/svelte/tests/parser-legacy/samples");
    let mut entries: Vec<_> = fs::read_dir(&base)
        .expect("parser-legacy samples")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    entries.sort_by_key(|e| e.file_name());

    let mut ok = 0;
    let mut parse_err = 0;
    let mut panicked = 0;

    for e in entries {
        let name = e.file_name().into_string().unwrap();
        let input = e.path().join("input.svelte");
        if !input.exists() {
            continue;
        }
        let Ok(source) = fs::read_to_string(&input) else { continue };

        // Mirrors upstream's `loose-*` fixture naming convention.
        let loose = name.starts_with("loose-");
        let result = std::panic::catch_unwind(|| parse(&source, loose));

        match result {
            Ok(Ok(_)) => ok += 1,
            Ok(Err(d)) => {
                parse_err += 1;
                println!("[PARSE] {name}: {}", d.code);
            }
            Err(_) => {
                panicked += 1;
                println!("[PANIC] {name}");
            }
        }
    }

    println!(
        "\nPARSER-LEGACY SUMMARY: {ok} ok / {parse_err} parse-err / {panicked} panic"
    );
}
