//! Sweep `packages/svelte/tests/hydration/samples/*` through the Rust
//! client transform and compare to upstream's `_output/client/main.svelte.js`
//! byte-for-byte. Marked `#[ignore]` so it doesn't run by default — invoke
//! via:
//!
//!   cargo test -p svelte_compiler --test hydration_sweep -- --ignored --nocapture

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
fn hydration_client_sweep() {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../packages/svelte/tests/hydration/samples");
    let mut entries: Vec<_> = fs::read_dir(&base)
        .expect("read hydration samples")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    entries.sort_by_key(|e| e.file_name());

    let mut ok = 0usize;
    let mut diff = 0usize;
    let mut err = 0usize;
    let mut panic_count = 0usize;
    for entry in entries {
        let name = entry.file_name().into_string().unwrap();
        let svelte = entry.path().join("main.svelte");
        let expected = entry
            .path()
            .join("_output/client/main.svelte.js");
        if !svelte.exists() || !expected.exists() {
            continue;
        }
        // Skip fixtures whose expected output requires non-default
        // compile options (hmr, dev) — compile()'s public surface
        // doesn't carry those yet.
        let config_path = entry.path().join("_config.js");
        if let Ok(cfg) = fs::read_to_string(&config_path) {
            if cfg.contains("hmr: true") || cfg.contains("hmr:true") || cfg.contains("dev: true") {
                println!("[skip] {name}");
                continue;
            }
        }
        let source = match fs::read_to_string(&svelte) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let exp = match fs::read_to_string(&expected) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let mut opts = CompileOptions::default();
        opts.module.generate = Some(Generate::Client);
        // Hydration fixtures all live in `main.svelte` — upstream derives
        // the component name from the filename, so the exported function
        // is always `Main`.
        let _ = snake_to_pascal(&name);
        let pname = "Main".to_string();
        let result = std::panic::catch_unwind(|| compile(&source, &pname, opts));
        match result {
            Ok(Ok(r)) if r.js.trim() == exp.trim() => {
                ok += 1;
                println!("[OK  ] {name}");
            }
            Ok(Ok(r)) => {
                diff += 1;
                let first = exp
                    .lines()
                    .zip(r.js.lines())
                    .enumerate()
                    .find(|(_, (a, b))| a != b)
                    .map(|(i, (a, b))| format!("L{i}\n    EXP: {a}\n    GOT: {b}"))
                    .unwrap_or_default();
                println!("[DIFF] {name}: {first}");
            }
            Ok(Err(d)) => {
                err += 1;
                println!("[ERR ] {name}: {}", d.code);
            }
            Err(_) => {
                panic_count += 1;
                println!("[PANIC] {name}");
            }
        }
    }
    println!(
        "\nHYDRATION SUMMARY: {ok} match / {diff} diff / {err} err / {panic_count} panic"
    );
}
