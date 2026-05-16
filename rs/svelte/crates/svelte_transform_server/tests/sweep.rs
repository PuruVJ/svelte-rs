//! Sweep over every snapshot fixture and report how many produce byte-equal
//! output. Marked `#[ignore]` so it doesn't run by default — invoke via
//! `cargo test -p svelte_transform_server --test sweep -- --ignored --nocapture`.

use std::fs;
use std::path::PathBuf;

use svelte_codegen_js::{default_visitors, print, PrintOptions};
use svelte_parse::parse;
use svelte_transform_server::server_component;

fn first_diff_line(expected: &str, got: &str) -> String {
    let e: Vec<&str> = expected.lines().collect();
    let g: Vec<&str> = got.lines().collect();
    for (i, (a, b)) in e.iter().zip(g.iter()).enumerate() {
        if a != b {
            return format!(
                "  L{i}:\n    EXP: {a}\n    GOT: {b}"
            );
        }
    }
    if e.len() != g.len() {
        format!(
            "  same prefix, differ in length (exp={} got={})",
            e.len(),
            g.len()
        )
    } else {
        "  (trailing whitespace differs)".to_string()
    }
}

/// Upstream's component-name derivation (`utils/name_from_filename`):
/// `hello-world` → `Hello_world`. First letter uppercased, subsequent
/// hyphens become underscores. Mirrors `phases/2-analyze/index.js:name = ...`.
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
fn sweep_snapshot_fixtures() {
    let base = PathBuf::from(
        "../../../../packages/svelte/tests/snapshot/samples",
    );
    let mut match_count = 0usize;
    let mut diverge_count = 0usize;
    let mut error_count = 0usize;
    let mut fixtures: Vec<_> = fs::read_dir(&base)
        .expect("read snapshot samples")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    fixtures.sort_by_key(|e| e.file_name());

    for entry in fixtures {
        let name = entry.file_name().into_string().unwrap();
        let svelte_path = entry.path().join("index.svelte");
        let expected_path = entry.path().join("_expected/server/index.svelte.js");
        if !svelte_path.exists() || !expected_path.exists() {
            continue;
        }
        let source = match fs::read_to_string(&svelte_path) {
            Ok(s) => s,
            Err(e) => {
                println!("[ERR  ] {name}: read source: {e}");
                error_count += 1;
                continue;
            }
        };
        let expected = match fs::read_to_string(&expected_path) {
            Ok(s) => s,
            Err(e) => {
                println!("[ERR  ] {name}: read expected: {e}");
                error_count += 1;
                continue;
            }
        };

        let component = snake_to_pascal(&name);
        let result = std::panic::catch_unwind(|| {
            let root = parse(&source, false)?;
            let program = server_component(&root, &component);
            // Comments: prefer parsed-expected `__embedded_comments`
            // (parse-roundtrip fixtures), fall back to Root.comments.
            let comments: Vec<serde_json::Value> = program
                .get("__embedded_comments")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_else(|| {
                    root.comments
                        .iter()
                        .map(|c| serde_json::to_value(c).unwrap())
                        .collect()
                });
            let mut popts = PrintOptions::default();
            popts.comments = comments;
            let r = print(&program, &default_visitors(), &popts);
            Ok::<_, svelte_diagnostics::CompileDiagnostic>(r.code)
        });
        match result {
            Ok(Ok(got)) if got == expected => {
                println!("[MATCH] {name}");
                match_count += 1;
            }
            Ok(Ok(got)) => {
                let preview = first_diff_line(&expected, &got);
                println!("[DIFF ] {name} {preview}");
                diverge_count += 1;
            }
            Ok(Err(e)) => {
                println!("[ERR  ] {name}: {e:?}");
                error_count += 1;
            }
            Err(_) => {
                println!("[PANIC] {name}");
                error_count += 1;
            }
        }
    }

    println!(
        "\nSUMMARY: {match_count} match / {diverge_count} diverge / {error_count} error"
    );
}
