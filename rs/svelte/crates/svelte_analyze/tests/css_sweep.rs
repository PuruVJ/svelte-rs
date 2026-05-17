//! Sweep over every CSS fixture and report how many produce byte-equal
//! output. Marked `#[ignore]` so it doesn't run by default — invoke via
//! `cargo test -p svelte_analyze --test css_sweep -- --ignored --nocapture`.

use std::fs;
use std::path::PathBuf;

use svelte_analyze::{analyze_component, css_render::render_stylesheet};
use svelte_parse::parse;

const HASH: &str = "svelte-xyz";

#[test]
#[ignore]
fn debug_basic() {
    let src = fs::read_to_string("../../../../packages/svelte/tests/css/samples/basic/input.svelte").unwrap();
    let root = parse(&src, false).unwrap();
    let mut analysis = analyze_component(root, None).unwrap();
    analysis.css_hash = "svelte-xyz".to_string();
    let sheet = analysis.css.as_ref().unwrap();
    eprintln!("content range: {}..{}", sheet.content.start, sheet.content.end);
    eprintln!("snippet=<<{}>>", &src[sheet.content.start as usize..sheet.content.end as usize]);
    eprintln!("scoped_elements={:?}", analysis.css_meta.scoped_elements);
    eprintln!("rule_metadata={:?}", analysis.css_meta.rule_metadata);
    eprintln!("complex_selector_metadata={:?}", analysis.css_meta.complex_selector_metadata);
    eprintln!("relative_selector_metadata={:?}", analysis.css_meta.relative_selector_metadata);
    let out = render_stylesheet(&src, sheet, &analysis.css_meta, "svelte-xyz");
    eprintln!("rendered=<<{}>>", out);
    panic!("dbg");
}

#[test]
#[ignore]
fn sweep_css_fixtures() {
    let base = PathBuf::from("../../../../packages/svelte/tests/css/samples");
    let mut match_count = 0usize;
    let mut diverge_count = 0usize;
    let mut error_count = 0usize;
    let mut fixtures: Vec<_> = fs::read_dir(&base)
        .expect("read css samples")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    fixtures.sort_by_key(|e| e.file_name());
    for entry in fixtures {
        let name = entry.file_name().into_string().unwrap();
        let svelte_path = entry.path().join("input.svelte");
        let expected_path = entry.path().join("expected.css");
        if !svelte_path.exists() || !expected_path.exists() {
            continue;
        }
        let source = match fs::read_to_string(&svelte_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let expected = match fs::read_to_string(&expected_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let result = std::panic::catch_unwind(|| {
            let root = parse(&source, false)?;
            let mut analysis = analyze_component(root, None)?;
            analysis.css_hash = HASH.to_string();
            let stylesheet = match analysis.css.as_ref() {
                Some(s) => s,
                None => return Ok::<_, svelte_diagnostics::CompileDiagnostic>(String::new()),
            };
            let rendered = render_stylesheet(&source, stylesheet, &analysis.css_meta, HASH);
            Ok(rendered)
        });
        match result {
            Ok(Ok(got)) if got.trim() == expected.trim() => {
                println!("[MATCH] {name}");
                match_count += 1;
            }
            Ok(Ok(got)) => {
                let first_diff = first_diff_line(expected.trim(), got.trim());
                println!("[DIFF ] {name}{first_diff}");
                diverge_count += 1;
            }
            Ok(Err(e)) => {
                println!("[ERR  ] {name}: {e:?}");
                error_count += 1;
            }
            Err(_) => {
                println!("[PANIC] {name}");
                error_count += 1;
            }
        }
    }
    println!(
        "\nCSS SUMMARY: {match_count} match / {diverge_count} diverge / {error_count} error"
    );
}

fn first_diff_line(expected: &str, got: &str) -> String {
    let e: Vec<&str> = expected.lines().collect();
    let g: Vec<&str> = got.lines().collect();
    for (i, (a, b)) in e.iter().zip(g.iter()).enumerate() {
        if a != b {
            return format!(
                "  L{i}:\n    EXP: {a}\n    GOT: {b}"
            );
        }
    }
    if e.len() != g.len() {
        format!(
            "  same prefix, differ in length (exp={} got={})",
            e.len(),
            g.len()
        )
    } else {
        "  (trailing whitespace differs)".to_string()
    }
}
