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

fn analyze_fixture_warnings(
    source: &str,
) -> Result<Vec<svelte_diagnostics::CompileDiagnostic>, svelte_diagnostics::CompileDiagnostic> {
    let ast = parse(source, false)?;
    let analysis = analyze_component(ast.root(), None)?;
    Ok(analysis.warnings)
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

        // Pre-read _config.js to detect compile-time `customElement: true`,
        // which suppresses `options_missing_custom_element` and
        // `custom_element_props_identifier` warnings (since `<svelte:options
        // customElement>` is then valid). Upstream threads this via
        // `options.customElement`; our analyzer doesn't yet take options, so
        // we filter after the fact.
        let config = fs::read_to_string(dir.join("_config.js")).unwrap_or_default();
        if config.contains("skip: true") {
            // Honor upstream's per-fixture skip marker.
            continue;
        }
        let custom_element_compile_option = config.contains("customElement: true");
        let runes_false = config.contains("runes: false");
        // Scan _config.js for `warningFilter` referencing `.includes(warning.code)`
        // and pull out the codes to silence. Mirrors upstream's options.warningFilter
        // for the simple case where the filter is `(w) => !['x', 'y'].includes(w.code)`.
        let warning_filter_codes: HashSet<String> = {
            let mut out = HashSet::new();
            if config.contains("warningFilter") && config.contains(".includes(warning.code)") {
                // Find the array literal between `[` and `]`.
                if let (Some(lb), Some(rb)) = (config.find('['), config.find(']')) {
                    if lb < rb {
                        let inner = &config[lb + 1..rb];
                        for tok in inner.split(',') {
                            let cleaned = tok.trim().trim_matches(|c: char| c == '\'' || c == '"');
                            if !cleaned.is_empty() {
                                out.insert(cleaned.to_string());
                            }
                        }
                    }
                }
            }
            out
        };

        let source = match fs::read_to_string(&input) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let result = std::panic::catch_unwind(|| analyze_fixture_warnings(&source));

        let mut analysis_warnings = match result {
            Ok(Ok(w)) => w,
            Ok(Err(e)) => {
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
            Err(_) => {
                error_count += 1;
                println!("[PANIC] {name}");
                continue;
            }
        };

        if custom_element_compile_option {
            analysis_warnings.retain(|w| {
                w.code != "options_missing_custom_element"
                    && w.code != "custom_element_props_identifier"
            });
        }
        if runes_false {
            analysis_warnings.retain(|w| {
                !matches!(
                    w.code,
                    "store_rune_conflict"
                        | "slot_element_deprecated"
                        | "svelte_component_deprecated"
                        | "event_directive_deprecated"
                )
            });
        }
        if !warning_filter_codes.is_empty() {
            analysis_warnings.retain(|w| !warning_filter_codes.contains(w.code));
        }
        let got_codes = diag_codes(&analysis_warnings);
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
