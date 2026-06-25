// Standalone harness: builds the in-tree compiler against a fixture
// and dumps per-phase timings. Run via `cargo run --release --bin bench_in_proc`.
use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("usage: bench_in_proc FIXTURE.svelte [iter]");
    let iter: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(5000);
    let source = std::fs::read_to_string(&path).expect("read");

    // Warm up
    for _ in 0..200 {
        let _ = run_once(&source);
    }

    let mut parse_ms = 0f64;
    let mut analyze_ms = 0f64;
    let mut transform_ms = 0f64;
    let mut codegen_ms = 0f64;

    let started = Instant::now();
    for _ in 0..iter {
        let (p, a, t, c) = run_once(&source);
        parse_ms += p;
        analyze_ms += a;
        transform_ms += t;
        codegen_ms += c;
    }
    let total = started.elapsed().as_secs_f64() * 1000.0;
    println!("file: {}", path);
    println!("iter: {}, total_wall={:.2}ms", iter, total);
    println!("    parse:     {:.4}ms/iter  ({:.1}%)", parse_ms / iter as f64, 100.0 * parse_ms / total);
    println!("    analyze:   {:.4}ms/iter  ({:.1}%)", analyze_ms / iter as f64, 100.0 * analyze_ms / total);
    println!("    transform: {:.4}ms/iter  ({:.1}%)", transform_ms / iter as f64, 100.0 * transform_ms / total);
    println!("    codegen:   {:.4}ms/iter  ({:.1}%)", codegen_ms / iter as f64, 100.0 * codegen_ms / total);
    let sum = parse_ms + analyze_ms + transform_ms + codegen_ms;
    println!("    sum:       {:.4}ms/iter  ({:.1}% of wall)", sum / iter as f64, 100.0 * sum / total);
}

fn run_once(source: &str) -> (f64, f64, f64, f64) {
    let t = Instant::now();
    let root = svelte_parse::parse(source, false).expect("parse");
    let p = t.elapsed().as_secs_f64() * 1000.0;

    let t = Instant::now();
    let _analysis = svelte_analyze::analyze_component(root.clone(), None).expect("analyze");
    let a = t.elapsed().as_secs_f64() * 1000.0;

    let t = Instant::now();
    let program = svelte_transform_client::client_component(&root, "Index");
    let tr = t.elapsed().as_secs_f64() * 1000.0;

    let t = Instant::now();
    let _ = svelte_codegen_js::print(
        &program,
        &svelte_codegen_js::default_visitors(),
        &svelte_codegen_js::PrintOptions::default(),
    );
    let c = t.elapsed().as_secs_f64() * 1000.0;

    (p, a, tr, c)
}
