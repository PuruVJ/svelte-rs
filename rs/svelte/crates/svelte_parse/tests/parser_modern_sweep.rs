//! Walk every fixture in `packages/svelte/tests/parser-modern/samples/` and
//! report whether our parser handles it without erroring. AST byte-equality
//! against upstream's `output.json` is deferred — would require a Root →
//! JSON serializer that mirrors upstream's exact key order + shape, which
//! is its own port. For now this sweep is a smoke check: did the parser
//! and analyze pipeline accept the fixture without panicking or erroring?

use std::fs;
use std::path::PathBuf;

use svelte_parse::parse;

#[test]
#[ignore]
fn sweep_parser_modern_fixtures() {
    let base = PathBuf::from("../../../../packages/svelte/tests/parser-modern/samples");
    let mut entries: Vec<_> = fs::read_dir(&base)
        .expect("parser-modern samples")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    entries.sort_by_key(|e| e.file_name());

    let mut ok = 0;
    let mut parse_err = 0;
    let mut analyze_err = 0;
    let mut panicked = 0;

    for e in entries {
        let name = e.file_name().into_string().unwrap();
        let input = e.path().join("input.svelte");
        if !input.exists() {
            continue;
        }
        let Ok(source) = fs::read_to_string(&input) else { continue };

        // Upstream enables `loose` mode for fixtures whose name starts with
        // `loose-`. Mirrors `parser-modern/test.ts:cwd.startsWith("loose-")`.
        let loose = name.starts_with("loose-");
        // Parser-only check — analyze isn't run in upstream's parser-modern
        // test runner, which calls just `parse()`.
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
    let _ = analyze_err;

    println!(
        "\nPARSER-MODERN SUMMARY: {ok} ok / {parse_err} parse-err / {panicked} panic"
    );
}
