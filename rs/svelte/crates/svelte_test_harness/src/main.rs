//! Fixture differ.
//!
//! Walks a directory under `packages/svelte/tests/`, runs each fixture through both
//! the existing JS compiler (via a Node probe — `probe/run.mjs`) and the in-tree
//! Rust compiler, and diffs the outputs.
//!
//! The JS compiler is the source of truth. Where Rust output diverges, JS wins
//! by definition (per the project's "code is the only source of truth" rule).

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// Path to the repo root.
/// CARGO_MANIFEST_DIR = `<repo>/rs/svelte/crates/svelte_test_harness` → walk up 4.
fn repo_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .ancestors()
        .nth(4)
        .expect("manifest must have 4 ancestors before repo root")
        .to_path_buf()
}

fn probe_script(root: &Path) -> PathBuf {
    root.join("rs/svelte/probe/run.mjs")
}

fn run_js_probe(root: &Path, mode: &str, fixture: &Path) -> Result<String, String> {
    let probe = probe_script(root);
    if !probe.exists() {
        return Err(format!("probe script not found: {}", probe.display()));
    }
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
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn usage() -> ! {
    eprintln!("usage: svelte_test_harness <mode> <fixture-path>");
    eprintln!();
    eprintln!("modes:");
    eprintln!("  parse      run parse() against the fixture's input.svelte");
    eprintln!("  compile    run compile() (client mode)");
    eprintln!("  compile-ssr run compile() with generate: 'server'");
    eprintln!();
    eprintln!("the fixture path is interpreted relative to the repo root.");
    std::process::exit(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() != 2 {
        usage();
    }
    let mode = &args[0];
    let fixture = PathBuf::from(&args[1]);
    let root = repo_root();

    let js_output = match run_js_probe(&root, mode, &fixture) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("JS probe failed: {e}");
            return ExitCode::from(2);
        }
    };

    // Phase 0: there is no Rust output yet. Print the JS probe result so we can
    // confirm end-to-end plumbing works against a real fixture.
    println!("--- JS compiler output (mode={mode}, fixture={}) ---", fixture.display());
    print!("{js_output}");

    ExitCode::SUCCESS
}
