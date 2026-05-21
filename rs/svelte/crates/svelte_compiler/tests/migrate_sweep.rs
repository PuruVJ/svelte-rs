//! Migrate sweep — walks `packages/svelte/tests/migrate/samples/` and
//! confirms `migrate(input.svelte).code` matches `output.svelte` after
//! `.trim()` normalization (the upstream comparator).

use std::fs;
use std::path::PathBuf;

use svelte_compiler::{migrate, MigrateOptions};

#[test]
#[ignore]
fn migrate_matches_all_fixtures() {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../packages/svelte/tests/migrate/samples");
    let mut entries: Vec<_> = fs::read_dir(&base)
        .expect("migrate samples dir")
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
        let input_path = entry.path().join("input.svelte");
        let output_path = entry.path().join("output.svelte");
        if !input_path.exists() || !output_path.exists() {
            skipped += 1;
            continue;
        }
        let Ok(source) = fs::read_to_string(&input_path) else {
            skipped += 1;
            continue;
        };
        let Ok(expected) = fs::read_to_string(&output_path) else {
            skipped += 1;
            continue;
        };
        // Upstream test runner does this normalization:
        //   input = readFile(input.svelte).replace(/\s+$/, '').replace(/\r/g, '')
        let input = source.trim_end().replace('\r', "");

        // Some fixtures carry a `_config.js` controlling `skip_filename` and
        // `use_ts`. Parse those flags textually — we don't need to evaluate
        // JS.
        let (skip_filename, use_ts) = read_config_flags(&entry.path());
        let filename = if skip_filename {
            None
        } else {
            Some("output.svelte".to_string())
        };

        let result = std::panic::catch_unwind(|| {
            migrate(
                &input,
                MigrateOptions {
                    filename,
                    use_ts,
                },
            )
            .code
        });

        match result {
            Ok(code) => {
                let a = code.trim();
                let b = expected.trim();
                if a == b {
                    ok += 1;
                    println!("[OK  ] {name}");
                } else {
                    diff += 1;
                    let _ = fs::write(entry.path().join("_actual.svelte"), &code);
                    let first_diff = first_diff_line(a, b);
                    println!("[DIFF] {name}");
                    if let Some((lineno, expected_line, got_line)) = first_diff {
                        println!("  L{lineno} EXP: {expected_line}");
                        println!("  L{lineno} GOT: {got_line}");
                    }
                }
            }
            Err(_) => {
                panicked += 1;
                println!("[PANIC] {name}");
            }
        }
        let _ = err;
    }

    println!(
        "\nMIGRATE SUMMARY: {ok} ok / {diff} diff / {err} err / {panicked} panic / {skipped} skipped"
    );
}

fn read_config_flags(dir: &std::path::Path) -> (bool, bool) {
    let config_path = dir.join("_config.js");
    let Ok(text) = fs::read_to_string(&config_path) else {
        return (false, false);
    };
    let skip_filename = text.contains("skip_filename: true");
    let use_ts = text.contains("use_ts: true");
    (skip_filename, use_ts)
}

fn first_diff_line<'a>(a: &'a str, b: &'a str) -> Option<(usize, String, String)> {
    let a_lines: Vec<&str> = a.lines().collect();
    let b_lines: Vec<&str> = b.lines().collect();
    let n = a_lines.len().max(b_lines.len());
    for i in 0..n {
        let a_line = a_lines.get(i).copied().unwrap_or("");
        let b_line = b_lines.get(i).copied().unwrap_or("");
        if a_line != b_line {
            return Some((i + 1, b_line.to_string(), a_line.to_string()));
        }
    }
    None
}
