//! Sweep over `packages/svelte/tests/compiler-errors/samples/` checking each
//! fixture parses to the expected error code. `_config.js` is a JS module —
//! we extract its expected error code via a simple regex (good enough for
//! the snapshot format upstream uses).

use std::fs;
use std::path::PathBuf;

use svelte_analyze::analyze_component;
use svelte_parse::parse;

fn extract_error_code(config: &str) -> Option<String> {
    // Scan for `code:` followed by `'identifier'`. Avoids a regex dep.
    let idx = config.find("code:")?;
    let after = &config[idx + 5..];
    let q = after.find('\'')?;
    let rest = &after[q + 1..];
    let end = rest.find('\'')?;
    let code = &rest[..end];
    if code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !code.is_empty() {
        Some(code.to_string())
    } else {
        None
    }
}

#[test]
#[ignore]
fn sweep_compiler_error_fixtures() {
    let base = PathBuf::from("../../../../packages/svelte/tests/compiler-errors/samples");
    let mut entries: Vec<_> = fs::read_dir(&base)
        .expect("samples dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    entries.sort_by_key(|e| e.file_name());

    let mut match_count = 0;
    let mut diverge_count = 0;
    let mut error_count = 0;

    for e in entries {
        let name = e.file_name().into_string().unwrap();
        let dir = e.path();
        let config_path = dir.join("_config.js");
        let src_path = if dir.join("main.svelte").exists() {
            dir.join("main.svelte")
        } else if dir.join("input.svelte").exists() {
            dir.join("input.svelte")
        } else {
            continue;
        };
        let config = match fs::read_to_string(&config_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let expected_code = match extract_error_code(&config) {
            Some(c) => c,
            None => continue,
        };
        let source = match fs::read_to_string(&src_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let result = std::panic::catch_unwind(|| {
            let root = parse(&source, false)?;
            analyze_component(&root, None)?;
            Ok::<_, svelte_diagnostics::CompileDiagnostic>(())
        });
        match result {
            Ok(Ok(_)) => {
                diverge_count += 1;
                println!("[MISS ] {name}: pipeline succeeded but expected {expected_code}");
            }
            Ok(Err(d)) => {
                if d.code == expected_code {
                    match_count += 1;
                } else {
                    println!("[DIFF ] {name}: got {}, expected {}", d.code, expected_code);
                    diverge_count += 1;
                }
            }
            Err(_) => {
                println!("[PANIC] {name}");
                error_count += 1;
            }
        }
    }

    println!(
        "\nCOMPILER-ERRORS SUMMARY: {match_count} match / {diverge_count} diverge / {error_count} error"
    );
}
