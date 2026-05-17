//! `svelte-rs` — small CLI wrapper for the Rust Svelte compiler. Reads
//! a .svelte source file and prints the compiled JS to stdout. Designed
//! for benchmarking and quick smoke testing — NOT a stable user-facing
//! tool yet.
//!
//! Usage:
//!   svelte-rs <input.svelte>                   # client generate
//!   svelte-rs --ssr <input.svelte>             # server generate
//!   svelte-rs --bench <N> <input.svelte>       # run compile N times,
//!                                                print only timing on
//!                                                last line (`elapsed_ms=…`)
//!   svelte-rs --name <Name> <input.svelte>     # override component name
//!                                                (default = file stem,
//!                                                Pascal-cased)
//!
//! Errors are written to stderr with a non-zero exit code.

use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use svelte_compiler::{compile, CompileOptions, Generate};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let mut ssr = false;
    let mut iterations: usize = 1;
    let mut bench = false;
    let mut name_override: Option<String> = None;
    let mut input: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--ssr" | "--server" => ssr = true,
            "--client" => ssr = false,
            "--bench" => {
                bench = true;
                i += 1;
                if i >= args.len() {
                    eprintln!("--bench requires a numeric iteration count");
                    return ExitCode::from(2);
                }
                iterations = args[i].parse().unwrap_or(1);
            }
            "--name" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("--name requires a value");
                    return ExitCode::from(2);
                }
                name_override = Some(args[i].clone());
            }
            "-h" | "--help" => {
                println!("svelte-rs <input.svelte> [--ssr] [--bench N] [--name X]");
                return ExitCode::SUCCESS;
            }
            other => {
                if input.is_some() {
                    eprintln!("unexpected positional arg: {}", other);
                    return ExitCode::from(2);
                }
                input = Some(other.to_string());
            }
        }
        i += 1;
    }
    let Some(path) = input else {
        eprintln!("usage: svelte-rs <input.svelte> [--ssr] [--bench N]");
        return ExitCode::from(2);
    };

    let source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("read {}: {}", path, e);
            return ExitCode::from(1);
        }
    };

    let name = name_override.unwrap_or_else(|| derive_component_name(&path));
    let mut options = CompileOptions::default();
    options.module.generate = Some(if ssr { Generate::Server } else { Generate::Client });

    if bench {
        // Warm-up: one extra compile not included in timing — makes
        // results steadier when iterations is small.
        let _ = compile(&source, &name, options.clone());
        let started = Instant::now();
        for _ in 0..iterations {
            match compile(&source, &name, options.clone()) {
                Ok(_) => {}
                Err(e) => {
                    eprintln!("compile error: {:?}", e);
                    return ExitCode::from(1);
                }
            }
        }
        let elapsed = started.elapsed();
        let per = elapsed.as_secs_f64() * 1000.0 / iterations as f64;
        println!(
            "iterations={} total_ms={:.3} per_ms={:.4}",
            iterations,
            elapsed.as_secs_f64() * 1000.0,
            per
        );
        return ExitCode::SUCCESS;
    }

    match compile(&source, &name, options) {
        Ok(result) => {
            print!("{}", result.js);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("compile error: {:?}", e);
            ExitCode::from(1)
        }
    }
}

/// Strip the extension from a path and Pascal-case its stem, mirroring how
/// `vite` derives component names from filenames.
fn derive_component_name(path: &str) -> String {
    let stem = Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Component");
    let mut out = String::with_capacity(stem.len());
    let mut upper_next = true;
    for ch in stem.chars() {
        if ch == '-' || ch == '_' || ch == '.' {
            upper_next = true;
        } else if upper_next {
            for c in ch.to_uppercase() {
                out.push(c);
            }
            upper_next = false;
        } else {
            out.push(ch);
        }
    }
    if out.is_empty() {
        out.push_str("Component");
    }
    out
}
