use criterion::{black_box, criterion_group, criterion_main, Criterion};
use svelte_magic_string::MagicString;

fn bench_construction(c: &mut Criterion) {
    let source = "a".repeat(10_000);
    c.bench_function("magic_string/new_10k", |b| {
        b.iter(|| MagicString::new(black_box(&source)));
    });
}

fn bench_overwrite(c: &mut Criterion) {
    let source = "abcdefghijklmnopqrstuvwxyz".repeat(100);
    c.bench_function("magic_string/overwrite_50x", |b| {
        b.iter(|| {
            let mut ms = MagicString::new(&source);
            for i in 0..50 {
                let start = i * 52;
                let end = start + 26;
                ms.overwrite(start, end, "REPLACED", false);
            }
            ms.to_string()
        });
    });
}

fn bench_append_prepend(c: &mut Criterion) {
    let source = "hello world";
    c.bench_function("magic_string/append_prepend_100x", |b| {
        b.iter(|| {
            let mut ms = MagicString::new(source);
            for _ in 0..100 {
                ms.append("_suffix");
                ms.prepend("prefix_");
            }
            ms.to_string()
        });
    });
}

fn bench_to_string(c: &mut Criterion) {
    let source = "x".repeat(5000);
    let mut ms = MagicString::new(&source);
    for i in (0..5000).step_by(100) {
        let end = (i + 50).min(5000);
        ms.overwrite(i, end, "REPLACED", false);
    }
    c.bench_function("magic_string/to_string_edited", |b| {
        b.iter(|| black_box(ms.to_string()));
    });
}

fn bench_sourcemap(c: &mut Criterion) {
    let source = "x".repeat(2000);
    let mut ms = MagicString::new(&source);
    for i in (0..2000).step_by(100) {
        let end = (i + 50).min(2000);
        ms.overwrite(i, end, "REPLACED", false);
    }
    c.bench_function("magic_string/generate_decoded_map", |b| {
        b.iter(|| {
            ms.generate_decoded_map(svelte_magic_string::GenerateMapOptions {
                file: Some("test.js".to_string()),
                source: Some("test.svelte".to_string()),
                include_content: true,
                hires: svelte_magic_string::Hires::Off,
            })
        });
    });
}

criterion_group!(
    benches,
    bench_construction,
    bench_overwrite,
    bench_append_prepend,
    bench_to_string,
    bench_sourcemap,
);
criterion_main!(benches);
