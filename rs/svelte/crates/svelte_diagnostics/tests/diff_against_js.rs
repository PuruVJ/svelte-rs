//! Differential test: assert every Rust-generated diagnostic produces the
//! same `code` + `message` string as the JS `errors.js` / `warnings.js` for
//! the same arguments.
//!
//! Uses `rs/svelte/probe/errors-sample.mjs` as the JS oracle. To extend
//! coverage, add cases to that script.

use std::path::PathBuf;
use std::process::Command;

use svelte_diagnostics::{errors, CompileDiagnostic};

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = `<repo>/rs/svelte/crates/svelte_diagnostics` → walk up 4.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .ancestors()
        .nth(4)
        .expect("manifest dir must have 4 ancestors before repo root")
        .to_path_buf()
}

fn run_js_oracle() -> serde_json::Value {
    let root = repo_root();
    let probe = root.join("rs/svelte/probe/errors-sample.mjs");
    let output = Command::new("node")
        .arg(&probe)
        .current_dir(&root)
        .output()
        .expect("node must be on PATH for this test");
    if !output.status.success() {
        panic!(
            "JS probe failed (status={}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    serde_json::from_slice(&output.stdout).expect("oracle JSON must be valid")
}

/// Translate a `(name, args)` JS case into the corresponding Rust call.
fn rust_call(name: &str, args: &[&str]) -> Option<CompileDiagnostic> {
    Some(match (name, args) {
        ("options_invalid_value", [a]) => errors::options_invalid_value(None, a),
        ("options_removed", [a]) => errors::options_removed(None, a),
        ("options_unrecognised", [a]) => errors::options_unrecognised(None, a),
        ("bind_invalid_name", [a]) => errors::bind_invalid_name(None, a, None),
        ("bind_invalid_name", [a, b]) => errors::bind_invalid_name(None, a, Some(b)),
        ("bind_invalid_parens", [a]) => errors::bind_invalid_parens(None, a),
        ("bind_invalid_target", [a, b]) => errors::bind_invalid_target(None, a, b),
        ("bind_invalid_expression", []) => errors::bind_invalid_expression(None),
        ("bind_invalid_value", []) => errors::bind_invalid_value(None),
        ("bind_group_invalid_expression", []) => errors::bind_group_invalid_expression(None),
        ("bind_group_invalid_snippet_parameter", []) => {
            errors::bind_group_invalid_snippet_parameter(None)
        }
        ("bindable_invalid_location", []) => errors::bindable_invalid_location(None),
        _ => return None,
    })
}

#[test]
fn rust_errors_match_js_byte_for_byte() {
    let oracle = run_js_oracle();
    let cases = oracle.as_array().expect("oracle must be an array");
    assert!(!cases.is_empty(), "oracle must yield cases");

    let mut failures = Vec::<String>::new();
    let mut checked = 0_usize;

    for case in cases {
        let kind = case["kind"].as_str().unwrap_or("");
        if kind != "error" {
            continue;
        }
        let name = case["name"].as_str().unwrap();
        let args: Vec<&str> = case["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();

        let Some(rust_diag) = rust_call(name, &args) else {
            failures.push(format!(
                "no Rust handler in test for ({}, {:?}) — extend `rust_call`",
                name, args
            ));
            continue;
        };

        let expected_code = case["code"].as_str().unwrap();
        let expected_message = case["message"].as_str().unwrap();

        if rust_diag.code != expected_code {
            failures.push(format!(
                "code mismatch for {}({:?}): rust={} js={}",
                name, args, rust_diag.code, expected_code
            ));
        }
        if rust_diag.message != expected_message {
            failures.push(format!(
                "message mismatch for {}({:?}):\n  rust: {:?}\n  js  : {:?}",
                name, args, rust_diag.message, expected_message
            ));
        }
        checked += 1;
    }

    assert!(checked > 0, "no error cases were checked");
    if !failures.is_empty() {
        panic!(
            "{} differential failures:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}
