// Standalone harness: builds the in-tree compiler against a fixture
// and dumps per-phase timings. Run via `cargo run --release --bin bench_in_proc`.
use std::cell::Cell;
use std::time::Instant;

thread_local! {
    static SPLIT_CONVERT_MS: Cell<f64> = const { Cell::new(0.0) };
    static SPLIT_PRINT_MS: Cell<f64> = const { Cell::new(0.0) };
}

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
    let convert = SPLIT_CONVERT_MS.with(|c| c.get());
    let printout = SPLIT_PRINT_MS.with(|p| p.get());
    println!("       (convert: {:.4}ms/iter, print: {:.4}ms/iter)",
        convert / iter as f64, printout / iter as f64);
    let sum = parse_ms + analyze_ms + transform_ms + codegen_ms;
    println!("    sum:       {:.4}ms/iter  ({:.1}% of wall)", sum / iter as f64, 100.0 * sum / total);
}

fn run_once(source: &str) -> (f64, f64, f64, f64) {
    let t = Instant::now();
    let root = svelte_parse::parse(source, false).expect("parse");
    let p = t.elapsed().as_secs_f64() * 1000.0;

    let t = Instant::now();
    let _analysis = svelte_analyze::analyze_component(&root, None).expect("analyze");
    let a = t.elapsed().as_secs_f64() * 1000.0;

    let t = Instant::now();
    let has_script_or_css =
        root.instance.is_some() || root.module.is_some() || root.css.is_some();
    let typed = if has_script_or_css {
        svelte_transform_server::try_typed_server_component(root, "Index")
    } else {
        svelte_transform_server::try_typed_server(&root, "Index")
            .or_else(|| svelte_transform_server::try_typed_server_component(root, "Index"))
    }
    .or_else(|| {
        let root = svelte_parse::parse(source, false).expect("parse");
        svelte_transform_client::try_typed_client(&root, "Index").or_else(|| {
            svelte_transform_client::try_typed_client_component(&root, "Index")
        })
    })
    .expect("no typed transform handles this fixture yet");
    let tr = t.elapsed().as_secs_f64() * 1000.0;
    let convert_ms = 0.0;

    let t = Instant::now();
    let _ = svelte_codegen_js::print_typed(
        &typed,
        &svelte_codegen_js::TypedPrintOptions::default(),
    );
    let print_ms = t.elapsed().as_secs_f64() * 1000.0;
    let c = convert_ms + print_ms;
    SPLIT_CONVERT_MS.with(|c| c.set(c.get() + convert_ms));
    SPLIT_PRINT_MS.with(|p| p.set(p.get() + print_ms));

    (p, a, tr, c)
}
