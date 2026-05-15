//! Walk every fixture in `packages/svelte/tests/validator/samples/` and check
//! that our analyzer emits the same warning/error set as upstream.
//!
//! Each fixture has:
//! - `input.svelte` — the source.
//! - `warnings.json` — expected warnings (array of `{code, message, start, end}`).
//! - optionally `errors.json` — expected hard errors.
//!
//! We count match / diverge / error and print per-fixture status. Marked
//! `#[ignore]` so it doesn't run by default — invoke with `--ignored`.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use serde_json::Value;
use svelte_analyze::analyze_component;
use svelte_parse::parse;

#[derive(Debug)]
struct ExpectedDiag {
    code: String,
}

fn read_expected(path: &PathBuf) -> Vec<ExpectedDiag> {
    let s = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let arr: Vec<Value> = serde_json::from_str(&s).unwrap_or_default();
    arr.into_iter()
        .filter_map(|v| {
            let code = v.get("code")?.as_str()?.to_string();
            Some(ExpectedDiag { code })
        })
        .collect()
}

fn diag_codes(diags: &[svelte_diagnostics::CompileDiagnostic]) -> HashSet<String> {
    diags.iter().map(|d| d.code.to_string()).collect()
}

#[test]
#[ignore]
fn sweep_validator_fixtures() {
    let base = PathBuf::from("../../../../packages/svelte/tests/validator/samples");
    let mut entries: Vec<_> = fs::read_dir(&base)
        .expect("read validator samples")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    entries.sort_by_key(|e| e.file_name());

    let mut match_count = 0usize;
    let mut diverge_count = 0usize;
    let mut error_count = 0usize;
    let mut missing_codes: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    for e in entries {
        let name = e.file_name().into_string().unwrap();
        let dir = e.path();
        let input = dir.join("input.svelte");
        if !input.exists() {
            continue;
        }
        let warnings_path = dir.join("warnings.json");
        let errors_path = dir.join("errors.json");
        let expected_warnings = read_expected(&warnings_path);
        let expected_errors = read_expected(&errors_path);

        let source = match fs::read_to_string(&input) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let result = std::panic::catch_unwind(|| {
            let root = parse(&source, false)?;
            let analysis = analyze_component(root, None);
            Ok::<_, svelte_diagnostics::CompileDiagnostic>(analysis)
        });

        let analysis = match result {
            Ok(Ok(Ok(a))) => a,
            Ok(Ok(Err(e))) => {
                // Compile error path — check it matches expected errors.
                let got_codes: HashSet<String> = std::iter::once(e.code.to_string()).collect();
                let expected_codes: HashSet<String> = expected_errors
                    .iter()
                    .map(|d| d.code.clone())
                    .collect();
                if got_codes == expected_codes {
                    match_count += 1;
                    println!("[MATCH] {name} (error)");
                } else {
                    diverge_count += 1;
                    println!(
                        "[DIFF ] {name}: got error code {:?}, expected {:?}",
                        got_codes, expected_codes
                    );
                }
                continue;
            }
            Ok(Err(parse_err)) => {
                // Compile error from analyze - shouldn't happen with the
                // outer Ok wrapper. Keep symmetric.
                error_count += 1;
                println!("[ERR  ] {name}: analyze: {parse_err:?}");
                continue;
            }
            Err(_) => {
                error_count += 1;
                println!("[PANIC] {name}");
                continue;
            }
        };

        let got_codes = diag_codes(&analysis.warnings);
        let expected_codes: HashSet<String> = expected_warnings
            .iter()
            .map(|d| d.code.clone())
            .collect();

        if got_codes == expected_codes {
            match_count += 1;
            // suppress matches in output
        } else {
            diverge_count += 1;
            let missing: Vec<_> = expected_codes.difference(&got_codes).collect();
            let extra: Vec<_> = got_codes.difference(&expected_codes).collect();
            for m in &missing {
                *missing_codes.entry((*m).clone()).or_insert(0) += 1;
            }
            println!(
                "[DIFF ] {name}: missing={:?} extra={:?}",
                missing, extra
            );
        }
    }

    println!(
        "\nVALIDATOR SUMMARY: {match_count} match / {diverge_count} diverge / {error_count} error"
    );
    let mut missing_sorted: Vec<_> = missing_codes.into_iter().collect();
    missing_sorted.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    println!("\nTop missing codes:");
    for (code, n) in missing_sorted.iter().take(20) {
        println!("  {n:>4} {code}");
    }
}
