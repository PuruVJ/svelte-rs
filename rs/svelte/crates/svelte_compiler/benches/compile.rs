use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

fn load_fixture(name: &str) -> String {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let path = format!(
        "{}/../../../../packages/svelte/tests/snapshot/samples/{}/index.svelte",
        manifest, name
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("failed to load {}: {}", path, e))
}

fn bench_parse(c: &mut Criterion) {
    let fixtures = [
        ("hello-world", "hello-world"),
        ("each-string-template", "each-string-template"),
        ("skip-static-subtree", "skip-static-subtree"),
        ("props-identifier", "props-identifier"),
        ("function-prop-no-getter", "function-prop-no-getter"),
        ("class-state-field", "class-state-field-constructor-assignment"),
    ];

    let mut group = c.benchmark_group("parse");
    for (label, name) in &fixtures {
        let source = load_fixture(name);
        group.bench_with_input(BenchmarkId::new("parse", label), &source, |b, src| {
            b.iter(|| svelte_parse::parse(black_box(src), false).unwrap());
        });
    }
    group.finish();
}

fn bench_analyze(c: &mut Criterion) {
    let fixtures = [
        ("hello-world", "hello-world"),
        ("skip-static-subtree", "skip-static-subtree"),
        ("props-identifier", "props-identifier"),
        ("class-state-field", "class-state-field-constructor-assignment"),
    ];

    let mut group = c.benchmark_group("analyze");
    for (label, name) in &fixtures {
        let source = load_fixture(name);
        let root = svelte_parse::parse(&source, false).unwrap();
        group.bench_with_input(BenchmarkId::new("analyze", label), &root, |b, root| {
            b.iter(|| {
                svelte_analyze::analyze_component(black_box(root), None).unwrap()
            });
        });
    }
    group.finish();
}

fn bench_transform_client(c: &mut Criterion) {
    let fixtures = [
        ("hello-world", "hello-world"),
        ("each-string-template", "each-string-template"),
        ("skip-static-subtree", "skip-static-subtree"),
        ("props-identifier", "props-identifier"),
        ("function-prop-no-getter", "function-prop-no-getter"),
        ("class-state-field", "class-state-field-constructor-assignment"),
    ];

    let mut group = c.benchmark_group("transform_client");
    for (label, name) in &fixtures {
        let source = load_fixture(name);
        let root = svelte_parse::parse(&source, false).unwrap();
        group.bench_with_input(BenchmarkId::new("transform", label), &root, |b, root| {
            b.iter(|| {
                let r = black_box(root);
                svelte_transform_client::try_typed_client(r, "Index")
                    .or_else(|| svelte_transform_client::try_typed_client_component(r, "Index"))
            });
        });
    }
    group.finish();
}

fn bench_transform_server(c: &mut Criterion) {
    let fixtures = [
        ("hello-world", "hello-world"),
        ("each-string-template", "each-string-template"),
        ("skip-static-subtree", "skip-static-subtree"),
        ("props-identifier", "props-identifier"),
    ];

    let mut group = c.benchmark_group("transform_server");
    for (label, name) in &fixtures {
        let source = load_fixture(name);
        let root = svelte_parse::parse(&source, false).unwrap();
        group.bench_with_input(BenchmarkId::new("transform", label), &source, |b, source| {
            b.iter(|| {
                let root = svelte_parse::parse(source, false).unwrap();
                black_box(
                    svelte_transform_server::try_typed_server(&root, "Index")
                        .or_else(|| {
                            let root = svelte_parse::parse(source, false).unwrap();
                            svelte_transform_server::try_typed_server_component(root, "Index")
                        })
                )
            });
        });
    }
    group.finish();
}

fn bench_codegen(c: &mut Criterion) {
    let fixtures = [
        ("hello-world", "hello-world"),
        ("each-string-template", "each-string-template"),
        ("skip-static-subtree", "skip-static-subtree"),
        ("props-identifier", "props-identifier"),
    ];

    let mut group = c.benchmark_group("codegen");
    for (label, name) in &fixtures {
        let source = load_fixture(name);
        let root = svelte_parse::parse(&source, false).unwrap();
        let typed = svelte_transform_server::try_typed_server(&root, "Index")
            .or_else(|| {
                let root = svelte_parse::parse(&source, false).unwrap();
                svelte_transform_server::try_typed_server_component(root, "Index")
            })
            .or_else(|| {
                let root = svelte_parse::parse(&source, false).unwrap();
                svelte_transform_client::try_typed_client(&root, "Index")
            })
            .or_else(|| {
                let root = svelte_parse::parse(&source, false).unwrap();
                svelte_transform_client::try_typed_client_component(&root, "Index")
            })
            .or_else(|| {
                let root = svelte_parse::parse(&source, false).unwrap();
                svelte_transform_client::try_typed_client_walker(root, "Index")
            });
        if let Some(ref program) = typed {
            group.bench_with_input(BenchmarkId::new("codegen", label), program, |b, prog| {
                b.iter(|| {
                    svelte_codegen_js::print_typed(
                        black_box(prog),
                        &svelte_codegen_js::TypedPrintOptions::default(),
                    )
                });
            });
        }
    }
    group.finish();
}

fn bench_end_to_end_client(c: &mut Criterion) {
    let fixtures = [
        ("hello-world", "hello-world"),
        ("each-string-template", "each-string-template"),
        ("skip-static-subtree", "skip-static-subtree"),
        ("props-identifier", "props-identifier"),
        ("function-prop-no-getter", "function-prop-no-getter"),
        ("class-state-field", "class-state-field-constructor-assignment"),
    ];

    let mut group = c.benchmark_group("end_to_end_client");
    for (label, name) in &fixtures {
        let source = load_fixture(name);
        group.bench_with_input(BenchmarkId::new("compile", label), &source, |b, src| {
            b.iter(|| {
                let mut opts = svelte_compiler::CompileOptions::default();
                opts.module.generate = Some(svelte_compiler::Generate::Client);
                svelte_compiler::compile(black_box(src), "Index", opts)
            });
        });
    }
    group.finish();
}

fn bench_end_to_end_server(c: &mut Criterion) {
    let fixtures = [
        ("hello-world", "hello-world"),
        ("each-string-template", "each-string-template"),
        ("skip-static-subtree", "skip-static-subtree"),
        ("props-identifier", "props-identifier"),
    ];

    let mut group = c.benchmark_group("end_to_end_server");
    for (label, name) in &fixtures {
        let source = load_fixture(name);
        group.bench_with_input(BenchmarkId::new("compile", label), &source, |b, src| {
            b.iter(|| {
                let mut opts = svelte_compiler::CompileOptions::default();
                opts.module.generate = Some(svelte_compiler::Generate::Server);
                svelte_compiler::compile(black_box(src), "Index", opts)
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_parse,
    bench_analyze,
    bench_transform_client,
    bench_transform_server,
    bench_codegen,
    bench_end_to_end_client,
    bench_end_to_end_server,
);
criterion_main!(benches);
