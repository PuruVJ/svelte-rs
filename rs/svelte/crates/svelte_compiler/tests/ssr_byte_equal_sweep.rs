//! Server-side-rendering byte-equality sweep.
//!
//! For every fixture in `packages/svelte/tests/server-side-rendering/
//! samples/`, compile `main.svelte` with `Generate::Server` and diff our
//! output against `_output/server/main.svelte.js`. Reports match/diff/err
//! counts. Marked `#[ignore]` so it doesn't run by default.
//!
//! Sibling to `ssr_smoke_sweep`: smoke checks "compiles + imports match";
//! this is the strict byte-equal version that mirrors what upstream's
//! own test driver checks.

use std::fs;
use std::path::PathBuf;

use svelte_compiler::{compile, CompileOptions, Generate};

#[test]
#[ignore]
fn ssr_byte_equal_all_fixtures() {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../packages/svelte/tests/server-side-rendering/samples");
    let mut entries: Vec<_> = fs::read_dir(&base)
        .expect("ssr samples dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    entries.sort_by_key(|e| e.file_name());

    let mut ok = 0usize;
    let mut diff = 0usize;
    let mut err = 0usize;
    let mut panicked = 0usize;
    let mut skipped = 0usize;

    for entry in entries {
        let name = entry.file_name().into_string().unwrap();
        let svelte = entry.path().join("main.svelte");
        let expected_path = entry.path().join("_output/server/main.svelte.js");
        if !svelte.exists() || !expected_path.exists() {
            skipped += 1;
            continue;
        }
        let source = match fs::read_to_string(&svelte) {
            Ok(s) => s,
            Err(_) => { skipped += 1; continue; }
        };
        let expected = match fs::read_to_string(&expected_path) {
            Ok(s) => s,
            Err(_) => { skipped += 1; continue; }
        };
        let _cfg = fs::read_to_string(entry.path().join("_config.js")).unwrap_or_default();

        let fname = format!(
            "packages/svelte/tests/server-side-rendering/samples/{}/main.svelte",
            name
        );
        let result = std::panic::catch_unwind(|| {
            let mut opts = CompileOptions::default();
            opts.module.generate = Some(Generate::Server);
            opts.module.experimental.async_ = true;
            opts.module.filename = Some(fname.clone());
            compile(&source, "Main", opts)
        });

        match result {
            Ok(Ok(r)) if r.js.trim() == expected.trim() => {
                ok += 1;
                println!("[OK  ] {name}");
            }
            Ok(Ok(r)) => {
                diff += 1;
                let (l_exp, l_got) = expected
                    .lines()
                    .zip(r.js.lines())
                    .enumerate()
                    .find(|(_, (a, b))| a != b)
                    .map(|(i, (a, b))| (format!("L{i}: {a}"), format!("L{i}: {b}")))
                    .unwrap_or_else(|| {
                        let exp_len = expected.lines().count();
                        let got_len = r.js.lines().count();
                        (
                            format!("(len {exp_len})"),
                            format!("(len {got_len})"),
                        )
                    });
                println!("[DIFF] {name}\n  EXP: {l_exp}\n  GOT: {l_got}");
            }
            Ok(Err(d)) => {
                err += 1;
                println!("[ERR ] {name}: {}", d.code);
            }
            Err(_) => {
                panicked += 1;
                println!("[PANIC] {name}");
            }
        }
    }

    println!(
        "\nSSR BYTE-EQUAL SUMMARY: {ok} ok / {diff} diff / {err} err / {panicked} panic / {skipped} skipped"
    );
}
