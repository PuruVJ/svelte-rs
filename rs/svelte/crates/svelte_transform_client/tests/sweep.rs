//! Client-side snapshot sweep. Like the server sweep — produces a count and
//! per-fixture status. Run via:
//! `cargo test -p svelte_transform_client --test sweep -- --ignored --nocapture`.

use std::fs;
use std::path::PathBuf;

use svelte_codegen_js::{default_visitors, print, PrintOptions};
use svelte_parse::parse;
use svelte_transform_client::{client_component_with_options, ClientOptions, FragmentsMode};

fn snake_to_pascal(name: &str) -> String {
    let mut chars = name.chars();
    let head = match chars.next() {
        Some(c) => c.to_ascii_uppercase().to_string(),
        None => String::new(),
    };
    let tail: String = chars.map(|c| if c == '-' { '_' } else { c }).collect();
    format!("{head}{tail}")
}

/// Extract a few compile options from a fixture's `_config.js`. Cheaply
/// regex-style — most fixtures use a consistent shape.
fn parse_config(config: &str) -> ClientOptions {
    let mut opts = ClientOptions::default();
    if config.contains("hmr: true") || config.contains("hmr:true") {
        opts.hmr = true;
    }
    if config.contains("dev: true") || config.contains("dev:true") {
        opts.dev = true;
    }
    if config.contains("fragments: 'tree'") || config.contains("fragments:'tree'") {
        opts.fragments = FragmentsMode::Tree;
    }
    opts
}

#[test]
#[ignore]
fn sweep_snapshot_fixtures_client() {
    let base = PathBuf::from("../../../../packages/svelte/tests/snapshot/samples");
    let mut match_count = 0usize;
    let mut diverge_count = 0usize;
    let mut error_count = 0usize;
    let mut fixtures: Vec<_> = fs::read_dir(&base)
        .expect("read snapshot samples")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    fixtures.sort_by_key(|e| e.file_name());
    for entry in fixtures {
        let name = entry.file_name().into_string().unwrap();
        let svelte_path = entry.path().join("index.svelte");
        let expected_path = entry.path().join("_expected/client/index.svelte.js");
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
        let config_path = entry.path().join("_config.js");
        let options = if config_path.exists() {
            let cfg = fs::read_to_string(&config_path).unwrap_or_default();
            parse_config(&cfg)
        } else {
            ClientOptions::default()
        };
        let component = snake_to_pascal(&name);
        let result = std::panic::catch_unwind(|| {
            let root = parse(&source, false)?;
            let program = client_component_with_options(&root, &component, &options);
            let r = print(&program, &default_visitors(), &PrintOptions::default());
            Ok::<_, svelte_diagnostics::CompileDiagnostic>(r.code)
        });
        match result {
            Ok(Ok(got)) if got == expected => {
                println!("[MATCH] {name}");
                match_count += 1;
            }
            Ok(Ok(_)) => {
                println!("[DIFF ] {name}");
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
        "\nCLIENT SUMMARY: {match_count} match / {diverge_count} diverge / {error_count} error"
    );
}
