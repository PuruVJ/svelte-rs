//! Roundtrip integration test.
//!
//! Spawns the JS probe to get `parse(source, { modern: true })` output for
//! every parser-modern fixture, deserializes the JSON into `svelte_ast::Root`,
//! re-serializes it, and asserts deep equality with the original.
//!
//! This is the Phase 1 gate: it proves the Rust AST type system can losslessly
//! represent the wire format of the upstream parser across the full parser-
//! modern fixture suite.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;
use svelte_ast::Root;

fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.ancestors().nth(4).unwrap().to_path_buf()
}

fn run_parse(fixture: &str) -> Value {
    let root = repo_root();
    let output = Command::new("node")
        .arg(root.join("rs/svelte/probe/run.mjs"))
        .arg("parse")
        .arg(fixture)
        .current_dir(&root)
        .output()
        .expect("node must be available");
    if !output.status.success() {
        panic!(
            "probe failed on {}: {}",
            fixture,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    serde_json::from_slice(&output.stdout).expect("probe must emit valid JSON")
}

fn assert_roundtrip(fixture: &str) -> Result<(), String> {
    let original = run_parse(fixture);
    let root: Root = serde_json::from_value(original.clone()).map_err(|e| {
        format!(
            "deserialize failed: {e}\nJSON was: {}",
            serde_json::to_string_pretty(&original).unwrap()
        )
    })?;
    let reserialized = serde_json::to_value(&root).expect("must serialize");
    if original != reserialized {
        let orig_pretty = serde_json::to_string_pretty(&original).unwrap();
        let new_pretty = serde_json::to_string_pretty(&reserialized).unwrap();
        return Err(format!(
            "roundtrip mismatch\n--- original ---\n{orig_pretty}\n--- reserialized ---\n{new_pretty}"
        ));
    }
    Ok(())
}

/// Walks every parser-modern fixture and asserts roundtrip equivalence.
///
/// On failure, lists every fixture that did not round-trip — much more useful
/// than a one-off `assert` per fixture when iterating.
#[test]
fn all_parser_modern_fixtures_roundtrip() {
    let dir = repo_root().join("packages/svelte/tests/parser-modern/samples");
    let mut fixtures: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| {
            let e = e.ok()?;
            let path = e.path();
            if !path.is_dir() {
                return None;
            }
            // Sample dirs ALWAYS have an input.svelte file.
            if !path.join("input.svelte").exists() {
                return None;
            }
            Some(path.file_name()?.to_str()?.to_string())
        })
        .collect();
    fixtures.sort();

    let mut failures: Vec<(String, String)> = Vec::new();
    let mut passes = 0_usize;
    for name in &fixtures {
        let rel = format!("packages/svelte/tests/parser-modern/samples/{name}");
        match assert_roundtrip(&rel) {
            Ok(()) => passes += 1,
            Err(msg) => failures.push((name.clone(), msg)),
        }
    }

    if !failures.is_empty() {
        let summary = failures
            .iter()
            .map(|(name, _msg)| format!("  - {name}"))
            .collect::<Vec<_>>()
            .join("\n");
        let first_detail = &failures[0];
        panic!(
            "{} fixture(s) failed to roundtrip, {} passed.\nFailures:\n{}\n\nFirst failure ({}):\n{}",
            failures.len(),
            passes,
            summary,
            first_detail.0,
            first_detail.1
        );
    }
    assert!(passes > 0, "no fixtures were exercised");
    eprintln!("parser-modern: {passes} fixtures round-tripped cleanly");
}
