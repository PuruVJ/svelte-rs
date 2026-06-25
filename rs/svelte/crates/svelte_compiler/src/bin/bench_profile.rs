//! CPU profile a single fixture compile loop and print top symbols + flamegraph.
//!
//! Usage:
//!   cargo run --release --features profile -p svelte_compiler --bin bench_profile -- \
//!     skip-static-subtree server 50000
//!
//! Modes: `server` | `client`

use std::fs::File;
use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let fixture = std::env::args().nth(1).expect("usage: bench_profile FIXTURE server|client [iter]");
    let mode = std::env::args().nth(2).expect("usage: bench_profile FIXTURE server|client [iter]");
    let iter: usize = std::env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(20_000);

    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../packages/svelte/tests/snapshot/samples")
        .join(&fixture)
        .join("index.svelte");
    let source = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

    let mut opts = svelte_compiler::CompileOptions::default();
    opts.module.generate = Some(match mode.as_str() {
        "server" => svelte_compiler::Generate::Server,
        "client" => svelte_compiler::Generate::Client,
        other => panic!("mode must be server or client, got {other}"),
    });

    // Warm up JIT/code layout outside the profile window.
    for _ in 0..200 {
        let _ = svelte_compiler::compile(&source, "Index", opts.clone());
    }

    let guard = pprof::ProfilerGuard::new(1000).expect("profiler");
    let t0 = Instant::now();
    for _ in 0..iter {
        let _ = svelte_compiler::compile(&source, "Index", opts.clone()).expect("compile");
    }
    let wall_ms = t0.elapsed().as_secs_f64() * 1000.0;

    if let Ok(report) = guard.report().build() {
        let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/profile");
        std::fs::create_dir_all(&out_dir).ok();
        let svg = out_dir.join(format!("{fixture}-{mode}.svg"));
        let mut f = File::create(&svg).expect("create svg");
        report.flamegraph(&mut f).expect("flamegraph");
        println!("flamegraph: {}", svg.display());

        let mut entries: Vec<(String, isize)> = report
            .data
            .into_iter()
            .map(|(frames, count)| (format!("{frames:?}"), count))
            .collect();
        entries.sort_by(|a, b| b.1.cmp(&a.1));
        let total: isize = entries.iter().map(|(_, v)| *v).sum();
        println!(
            "fixture={fixture} mode={mode} iter={iter} wall={wall_ms:.2}ms ({:.4} ms/iter)",
            wall_ms / iter as f64
        );
        println!("top symbols (sample share):");
        for (name, count) in entries.iter().take(40) {
            let pct = 100.0 * (*count as f64) / total as f64;
            println!("  {pct:5.1}%  {name}");
        }
    }
}
