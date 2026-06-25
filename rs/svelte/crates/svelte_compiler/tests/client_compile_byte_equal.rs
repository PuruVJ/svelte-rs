//! Mirror of `server_compile_byte_equal.rs` for the client side.
//! Verifies `compile(source, Generate::Client)` is byte-identical to
//! the upstream snapshot fixture for every client sample. Fixtures whose
//! expected output depends on compile options (hmr, dev) are skipped —
//! those go through the legacy `client_component_with_options` path
//! which the upstream sweep test exercises directly.

use std::fs;
use std::path::PathBuf;

use svelte_compiler::{compile, CompileOptions, Generate};

fn snake_to_pascal(name: &str) -> String {
    let mut chars = name.chars();
    let head = match chars.next() {
        Some(c) => c.to_ascii_uppercase().to_string(),
        None => String::new(),
    };
    let tail: String = chars.map(|c| if c == '-' { '_' } else { c }).collect();
    format!("{head}{tail}")
}

#[test]
#[ignore]
fn client_compile_matches_all_fixtures() {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../packages/svelte/tests/snapshot/samples");
    let mut entries: Vec<_> = fs::read_dir(&base)
        .expect("read samples")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    entries.sort_by_key(|e| e.file_name());

    let mut ok = 0usize;
    let mut bad = 0usize;
    let mut skipped = 0usize;
    for entry in entries {
        let name = entry.file_name().into_string().unwrap();
        // Support `index.svelte` (default) and `main.svelte` (e.g.
        // dynamic-attributes-casing) — pick whichever exists.
        let (svelte, src_basename) = if entry.path().join("index.svelte").exists() {
            (entry.path().join("index.svelte"), "index.svelte")
        } else if entry.path().join("main.svelte").exists() {
            (entry.path().join("main.svelte"), "main.svelte")
        } else {
            continue;
        };
        let expected_path = entry
            .path()
            .join(format!("_expected/client/{src_basename}.js"));
        if !expected_path.exists() {
            continue;
        }
        // Skip fixtures whose expected output requires non-default
        // compile options (hmr, dev). compile()'s public surface
        // doesn't carry those yet.
        let config_path = entry.path().join("_config.js");
        if let Ok(cfg) = fs::read_to_string(&config_path) {
            if cfg.contains("hmr: true") || cfg.contains("hmr:true") || cfg.contains("dev: true")
            {
                skipped += 1;
                println!("[skip] {name}");
                continue;
            }
        }
        let source = fs::read_to_string(&svelte).unwrap_or_default();
        let expected = fs::read_to_string(&expected_path).unwrap_or_default();
        let cfg = fs::read_to_string(&config_path).unwrap_or_default();
        let mut opts = CompileOptions::default();
        opts.module.generate = Some(Generate::Client);
        if cfg.contains("async: true") {
            opts.module.experimental.async_ = true;
        }
        if cfg.contains("fragments: 'tree'") {
            opts.fragments = svelte_compiler::FragmentsStrategy::Tree;
        }
        // Component name: derived from the source file's basename
        // (so `main.svelte` → `Main`), matching upstream's behavior.
        let pname = if src_basename == "main.svelte" {
            "Main".to_string()
        } else {
            snake_to_pascal(&name)
        };
        let result = compile(&source, &pname, opts);
        match result {
            Ok(r) if r.js.trim() == expected.trim() => {
                ok += 1;
                println!("[OK  ] {name}");
            }
            Ok(r) => {
                bad += 1;
                let (l_exp, l_got) = expected
                    .lines()
                    .zip(r.js.lines())
                    .enumerate()
                    .find(|(_, (a, b))| a != b)
                    .map(|(i, (a, b))| (format!("L{i}: {a}"), format!("L{i}: {b}")))
                    .unwrap_or_else(|| ("?".into(), "?".into()));
                println!("[DIFF] {name}\n  EXP: {l_exp}\n  GOT: {l_got}");
            }
            Err(e) => {
                bad += 1;
                println!("[ERR ] {name}: {e:?}");
            }
        }
    }
    println!(
        "\nCLIENT compile() COVERAGE: {ok} / {} match ({skipped} options-dependent fixtures skipped)",
        ok + bad
    );
    assert_eq!(bad, 0, "{bad} client fixtures diverge");
}
