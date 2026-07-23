//! Verify `compile(source, Generate::Server)` produces byte-identical
//! output to the upstream snapshot fixture for every server sample.
//!
//! Why: the existing `svelte_transform_server` sweep test bypasses
//! `compile()` and calls `server_component` + `print()` directly with
//! the right `PrintOptions.comments`. This test exercises the full
//! `compile()` surface that downstream users hit via the WASM bridge.

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
fn server_compile_matches_all_fixtures() {
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
    for entry in entries {
        let name = entry.file_name().into_string().unwrap();
        let svelte = entry.path().join("index.svelte");
        let expected_path = entry.path().join("_expected/server/index.svelte.js");
        if !svelte.exists() || !expected_path.exists() {
            continue;
        }
        let source = fs::read_to_string(&svelte).unwrap_or_default();
        let expected = fs::read_to_string(&expected_path).unwrap_or_default();
        let config_path = entry.path().join("_config.js");
        let cfg = fs::read_to_string(&config_path).unwrap_or_default();
        let mut opts = CompileOptions::default();
        opts.module.generate = Some(Generate::Server);
        if cfg.contains("async: true") {
            opts.module.experimental.async_ = true;
        }
        opts.name = Some(snake_to_pascal(&name));
        let result = compile(&source, opts);
        match result {
            Ok(r) if r.js.code.trim() == expected.trim() => {
                ok += 1;
                println!("[OK  ] {name}");
            }
            Ok(r) => {
                bad += 1;
                let (l_exp, l_got) = expected
                    .lines()
                    .zip(r.js.code.lines())
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
    println!("\nSERVER compile() COVERAGE: {ok} / {} match", ok + bad);
    assert_eq!(bad, 0, "{bad} server fixtures diverge");
}
