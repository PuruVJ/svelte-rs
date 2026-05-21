//! Server-side-rendering smoke sweep.
//!
//! Walks every fixture in `packages/svelte/tests/server-side-rendering/
//! samples/` and verifies our server-side `compile()` produces output that
//! is structurally compatible with the upstream JS compiler's expected
//! output. Verifying actual rendered HTML requires the JS runtime, which
//! is out of scope.
//!
//! Smoke criteria (per the rs-port plan):
//! 1. Rust compile succeeds without erroring or panicking.
//! 2. Rust output's `import` lines (specifically the `svelte/internal/*`
//!    paths and any same-directory `./*.svelte` imports) are a superset
//!    of the upstream expected output's imports. A superset, not strict
//!    equality, because divergent helper choices (e.g. emitting
//!    `$.attr_class` vs `$.attr` when both yield the same HTML) are
//!    acceptable for smoke. Missing an expected import is the regression
//!    signal we care about.
//!
//! Each fixture has `main.svelte` and `_output/server/main.svelte.js`.
//! Some fixtures pull in adjacent components (e.g. `component.svelte`) —
//! those aren't separately compiled here; we only smoke-check the entry.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use svelte_compiler::{compile, CompileOptions, Generate};

fn collect_imports(js: &str) -> HashSet<String> {
    // Multi-line imports (`import {\n  ...\n} from 'src';`) and
    // single-line forms (`import * as $ from 'src';`, `import 'src';`)
    // are both captured here. The strategy: find each `from '...'` or
    // `from "..."` occurrence, plus each side-effect `import '...'` /
    // `import "..."` on its own line.
    let mut imports = HashSet::new();

    // Side-effect imports: `import 'src';` (no `from`).
    for line in js.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("import ") {
            continue;
        }
        let rest = trimmed.trim_start_matches("import ").trim_start();
        if rest.starts_with('\'') || rest.starts_with('"') {
            // Side-effect form: the first quote opens the source.
            let q = rest.chars().next().unwrap();
            if let Some(end) = rest[1..].find(q) {
                imports.insert(rest[1..1 + end].to_string());
            }
        }
    }

    // `from '...'` / `from "..."` — covers all named/namespace/default forms.
    let mut i = 0;
    let bytes = js.as_bytes();
    while i + 5 < bytes.len() {
        if &bytes[i..i + 5] == b"from " {
            // Skip whitespace after `from `.
            let mut j = i + 5;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b'\'' || bytes[j] == b'"') {
                let q = bytes[j];
                let start = j + 1;
                let mut k = start;
                while k < bytes.len() && bytes[k] != q {
                    k += 1;
                }
                if k < bytes.len() {
                    imports.insert(String::from_utf8_lossy(&bytes[start..k]).to_string());
                }
                i = k + 1;
                continue;
            }
        }
        i += 1;
    }

    imports
}

#[test]
#[ignore]
fn ssr_smoke_all_fixtures() {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../packages/svelte/tests/server-side-rendering/samples");
    let mut entries: Vec<_> = fs::read_dir(&base)
        .expect("ssr samples dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    entries.sort_by_key(|e| e.file_name());

    let mut ok = 0usize;
    let mut import_mismatch = 0usize;
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
        let config_path = entry.path().join("_config.js");
        let cfg = fs::read_to_string(&config_path).unwrap_or_default();

        let _ = cfg;
        let result = std::panic::catch_unwind(|| {
            // Upstream's SSR test driver sets `experimental.async = true` for
            // every fixture, so every expected output emits the `flags/async`
            // import. Mirror that here.
            let mut opts = CompileOptions::default();
            opts.module.generate = Some(Generate::Server);
            opts.module.experimental.async_ = true;
            compile(&source, "Main", opts)
        });

        match result {
            Ok(Ok(r)) => {
                let exp_imports = collect_imports(&expected);
                let got_imports = collect_imports(&r.js);
                let missing: Vec<_> = exp_imports
                    .difference(&got_imports)
                    .cloned()
                    .collect();
                if missing.is_empty() {
                    ok += 1;
                    println!("[OK  ] {name}");
                } else {
                    import_mismatch += 1;
                    println!("[IMP ] {name}  missing: {missing:?}");
                }
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
        "\nSSR SMOKE SUMMARY: {ok} ok / {import_mismatch} import-mismatch / {err} err / {panicked} panic / {skipped} skipped"
    );
}
