//! Debug helper: diff client output for one specific fixture.

use std::fs;
use std::path::PathBuf;
use svelte_codegen_js::{default_visitors, print, PrintOptions};
use svelte_parse::parse;
use svelte_transform_client::{client_component_with_options, ClientOptions};

fn diff(name: &str, component: &str) {
    let base = PathBuf::from(format!(
        "../../../../packages/svelte/tests/snapshot/samples/{name}"
    ));
    let src = fs::read_to_string(base.join("index.svelte")).unwrap();
    let expected =
        fs::read_to_string(base.join("_expected/client/index.svelte.js")).unwrap();
    let config = fs::read_to_string(base.join("_config.js")).unwrap_or_default();
    let mut options = ClientOptions::default();
    if config.contains("hmr: true") {
        options.hmr = true;
    }
    let root = parse(&src, false).unwrap();
    let prog = client_component_with_options(&root, component, &options);
    let r = print(&prog, &default_visitors(), &PrintOptions::default());
    println!("=== EXPECTED ===\n{expected}\n=== GOT ===\n{}", r.code);
    if r.code != expected {
        panic!("diff for {name}");
    }
}

#[test]
#[ignore]
fn diff_each_string_template() {
    diff("each-string-template", "Each_string_template");
}

#[test]
#[ignore]
fn diff_each_index_non_null() {
    diff("each-index-non-null", "Each_index_non_null");
}

#[test]
#[ignore]
fn diff_purity() {
    diff("purity", "Purity");
}

#[test]
#[ignore]
fn diff_svelte_element() {
    diff("svelte-element", "Svelte_element");
}
