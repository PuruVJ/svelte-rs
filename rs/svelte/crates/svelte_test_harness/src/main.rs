//! Fixture differ.
//!
//! Runs the in-tree Rust compiler and the upstream JS compiler (via the Node
//! probe at `rs/svelte/probe/run.mjs`) against the same fixture, and diffs
//! the JSON outputs. The JS compiler is the source of truth — where Rust
//! output diverges, JS wins by definition (per the project rule "code is the
//! only source of truth").
//!
//! Usage:
//!   svelte_test_harness <mode> <fixture-path>
//!   svelte_test_harness all-parser-modern        — walks the whole suite
//!
//! Modes:
//!   parse        — parse(input, { modern: true })
//!
//! `<fixture-path>` is interpreted relative to the repo root and may be a
//! directory containing input.svelte (or main.svelte) or a direct .svelte file.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde_json::Value;
use similar::TextDiff;
use svelte_compiler::{parse as rust_parse, ParseOptions};

fn repo_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir.ancestors().nth(4).unwrap().to_path_buf()
}

fn load_source(root: &Path, fixture: &Path) -> Result<(PathBuf, String), String> {
    let abs = if fixture.is_absolute() {
        fixture.to_path_buf()
    } else {
        root.join(fixture)
    };
    if !abs.exists() {
        return Err(format!("not found: {}", abs.display()));
    }
    if abs.is_dir() {
        for name in ["input.svelte", "main.svelte"] {
            let p = abs.join(name);
            if p.exists() {
                let s = fs::read_to_string(&p).map_err(|e| e.to_string())?;
                return Ok((p, s));
            }
        }
        return Err(format!("no input.svelte / main.svelte in {}", abs.display()));
    }
    let s = fs::read_to_string(&abs).map_err(|e| e.to_string())?;
    Ok((abs, s))
}

/// Apply the same source normalization the `parser-modern/test.ts` runner
/// applies before calling `parse()`. Mirrors lines 14-17 of that file.
fn normalize_source(s: &str) -> String {
    let no_cr = s.replace('\r', "");
    no_cr.trim_end().to_string()
}

fn run_js_probe(root: &Path, mode: &str, fixture: &Path) -> Result<Value, String> {
    let probe = root.join("rs/svelte/probe/run.mjs");
    let output = Command::new("node")
        .arg(&probe)
        .arg(mode)
        .arg(fixture)
        .current_dir(root)
        .output()
        .map_err(|e| format!("failed to spawn node: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "probe exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("probe JSON parse: {e}"))
}

fn run_rust(mode: &str, source: &str) -> Result<Value, String> {
    match mode {
        "parse" => {
            let root = rust_parse(
                &normalize_source(source),
                ParseOptions {
                    modern: true,
                    ..Default::default()
                },
            )
            .map_err(|d| format!("Rust parse error: {} ({})", d.message, d.code))?;
            serde_json::to_value(&root).map_err(|e| format!("Rust serialize: {e}"))
        }
        other => Err(format!("unsupported mode for Rust side: {other}")),
    }
}

/// Compare two JSON values and report whether they match.
fn diff(rust: &Value, js: &Value) -> Result<(), String> {
    if rust == js {
        return Ok(());
    }
    let rust_pretty = serde_json::to_string_pretty(rust).unwrap();
    let js_pretty = serde_json::to_string_pretty(js).unwrap();
    let text_diff = TextDiff::from_lines(&js_pretty, &rust_pretty);
    let mut out = String::new();
    for change in text_diff.iter_all_changes() {
        let sign = match change.tag() {
            similar::ChangeTag::Delete => "-",
            similar::ChangeTag::Insert => "+",
            similar::ChangeTag::Equal => " ",
        };
        out.push_str(&format!("{sign}{}", change));
    }
    Err(out)
}

fn run_single(root: &Path, mode: &str, fixture: &Path) -> Result<bool, String> {
    let (_path, source) = load_source(root, fixture)?;
    let rust = run_rust(mode, &source)?;
    let js = run_js_probe(root, mode, fixture)?;
    match diff(&rust, &js) {
        Ok(()) => Ok(true),
        Err(_d) => Ok(false),
    }
}

fn list_fixtures(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.join("input.svelte").exists() {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

fn run_all_parser_modern(root: &Path) -> ExitCode {
    run_suite(root, "parser-modern")
}

fn run_all_parser_legacy(root: &Path) -> ExitCode {
    run_suite(root, "parser-legacy")
}

fn run_suite(root: &Path, suite: &str) -> ExitCode {
    let dir = root.join(format!("packages/svelte/tests/{suite}/samples"));
    let fixtures = match list_fixtures(&dir) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("could not list {suite} samples: {e}");
            return ExitCode::from(2);
        }
    };

    let mut matched = 0_usize;
    let mut diverged = 0_usize;
    let mut errored = 0_usize;
    for fx in &fixtures {
        let name = fx.file_name().unwrap().to_string_lossy();
        match run_single(root, "parse", fx) {
            Ok(true) => {
                matched += 1;
                println!("ok      {suite}/{name}");
            }
            Ok(false) => {
                diverged += 1;
                println!("diff    {suite}/{name}");
            }
            Err(e) => {
                errored += 1;
                // Truncate multi-line error messages to a single line for the
                // suite walker — full diagnostics are available via the
                // single-fixture mode.
                let one_line = e.lines().next().unwrap_or("").to_string();
                println!("error   {suite}/{name} — {one_line}");
            }
        }
    }
    println!();
    println!(
        "{suite}: {} total: {matched} match, {diverged} diverge, {errored} error",
        fixtures.len()
    );
    if diverged == 0 && errored == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn usage() -> ! {
    eprintln!("usage:");
    eprintln!("  svelte_test_harness <mode> <fixture-path>");
    eprintln!("  svelte_test_harness rust <mode> <fixture-path>     (print Rust output only)");
    eprintln!("  svelte_test_harness all-parser-modern");
    eprintln!("  svelte_test_harness all-parser-legacy");
    eprintln!();
    eprintln!("modes: parse");
    std::process::exit(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let root = repo_root();
    match args.as_slice() {
        [single] if single == "all-parser-modern" => run_all_parser_modern(&root),
        [single] if single == "all-parser-legacy" => run_all_parser_legacy(&root),
        [cmd, mode, fixture] if cmd == "rust" => {
            let fixture = PathBuf::from(fixture);
            let (_p, source) = match load_source(&root, &fixture) {
                Ok(v) => v,
                Err(e) => { eprintln!("{e}"); return ExitCode::from(2); }
            };
            match run_rust(mode, &source) {
                Ok(v) => {
                    println!("{}", serde_json::to_string_pretty(&v).unwrap());
                    ExitCode::SUCCESS
                }
                Err(e) => { eprintln!("{e}"); ExitCode::from(2) }
            }
        }
        [mode, fixture] => {
            let fixture = PathBuf::from(fixture);
            match run_single(&root, mode, &fixture) {
                Ok(true) => {
                    println!("ok    {} ({})", fixture.display(), mode);
                    ExitCode::SUCCESS
                }
                Ok(false) => {
                    // Re-run to get the diff text for output.
                    let (_p, source) = load_source(&root, &fixture).unwrap();
                    let rust = run_rust(mode, &source).unwrap();
                    let js = run_js_probe(&root, mode, &fixture).unwrap();
                    if let Err(diff) = diff(&rust, &js) {
                        println!("diff  {} ({})", fixture.display(), mode);
                        print!("{diff}");
                    }
                    ExitCode::FAILURE
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::from(2)
                }
            }
        }
        _ => usage(),
    }
}
