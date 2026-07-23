//! Per-crate phase timing for one fixture (measured, not estimated).
//!
//! Usage:
//!   cargo run --release -p svelte_compiler --bin bench_phases -- skip-static-subtree server 5000

use std::time::Instant;

fn main() {
    let fixture = std::env::args().nth(1).expect("usage: bench_phases FIXTURE server|client [iter]");
    let mode = std::env::args().nth(2).expect("usage: bench_phases FIXTURE server|client|e2e [iter]");
    let iter: usize = std::env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(5000);

    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../packages/svelte/tests/snapshot/samples")
        .join(&fixture)
        .join("index.svelte");
    let source = std::fs::read_to_string(&path).unwrap();

    if mode == "e2e" {
        bench_e2e_compile(&fixture, &source, iter);
        return;
    }

    let mut opts = svelte_compiler::CompileOptions::default();
    opts.module.generate = Some(match mode.as_str() {
        "server" => svelte_compiler::Generate::Server,
        "client" => svelte_compiler::Generate::Client,
        other => panic!("mode must be server or client, got {other}"),
    });

    let mut parse_ms = 0.0;
    let mut analyze_ms = 0.0;
    let mut transform_ms = 0.0;
    let mut codegen_ms = 0.0;

    for _ in 0..iter {
        let t = Instant::now();
        let ast = svelte_parse::parse(&source, false).unwrap();
        let root = ast.root();
        parse_ms += t.elapsed().as_secs_f64() * 1000.0;

        let t = Instant::now();
        let _ = svelte_analyze::analyze_component(root, None).unwrap();
        analyze_ms += t.elapsed().as_secs_f64() * 1000.0;

        let t = Instant::now();
        let bump = svelte_transform_shared::compile_bump::CompileBump::new();
        let typed = match opts.module.generate {
            Some(svelte_compiler::Generate::Server) => {
                if root.instance.is_some() || root.module.is_some() || root.css.is_some() {
                    let arena_root =
                        svelte_parse::parse_in_arena(&bump.template, &source, false).unwrap();
                    svelte_transform_server::try_typed_server_component(arena_root, "Index", bump.bump())
                } else {
                    svelte_transform_server::try_typed_server(root, "Index", bump.bump()).or_else(|| {
                        let arena_root =
                            svelte_parse::parse_in_arena(&bump.template, &source, false).ok()?;
                        svelte_transform_server::try_typed_server_component(arena_root, "Index", bump.bump())
                    })
                }
            }
            Some(svelte_compiler::Generate::Client) | None => {
                let mut arena_root =
                    svelte_parse::parse_in_arena(&bump.template, &source, false).unwrap();
                svelte_transform_client::walker_fold_in_fragment(&mut arena_root.fragment);
                svelte_transform_client::try_typed_client(&arena_root, "Index", bump.bump())
                    .or_else(|| {
                        svelte_transform_client::try_typed_client_component(&arena_root, "Index", bump.bump())
                    })
                    .or_else(|| {
                        svelte_transform_client::try_typed_client_walker_with_filename(
                            arena_root,
                            "Index",
                            false,
                            None,
                            bump.bump(),
                        )
                    })
            }
        };
        transform_ms += t.elapsed().as_secs_f64() * 1000.0;

        let Some(typed) = typed else {
            eprintln!("transform returned None for {fixture} ({mode})");
            std::process::exit(1);
        };

        let t = Instant::now();
        let mut typed_opts = svelte_codegen_js::TypedPrintOptions::default();
        typed_opts.code_init_capacity =
            svelte_codegen_js::estimate_code_init_capacity(source.len());
        let _ = svelte_codegen_js::print_typed(&typed, &typed_opts);
        codegen_ms += t.elapsed().as_secs_f64() * 1000.0;
        let _ = bump;
    }

    let n = iter as f64;
    println!("fixture={fixture} mode={mode} iter={iter}");
    for (label, ms) in [
        ("parse", parse_ms / n),
        ("analyze", analyze_ms / n),
        ("transform", transform_ms / n),
        ("codegen", codegen_ms / n),
    ] {
        println!("  {label:10} {ms:.4} ms/iter");
    }
    let sum = (parse_ms + analyze_ms + transform_ms + codegen_ms) / n;
    println!("  sum        {sum:.4} ms/iter");
}

fn bench_e2e_compile(fixture: &str, source: &str, iter: usize) {
    let mut opts = svelte_compiler::CompileOptions::default();
    opts.module.generate = Some(svelte_compiler::Generate::Client);
    let mut total_ms = 0.0;
    for _ in 0..iter {
        let t = Instant::now();
        opts.name = Some("Index".to_string());
        let _ = svelte_compiler::compile(source, opts.clone()).unwrap();
        total_ms += t.elapsed().as_secs_f64() * 1000.0;
    }
    println!("fixture={fixture} mode=e2e iter={iter}");
    println!("  compile    {:.4} ms/iter", total_ms / iter as f64);
}
