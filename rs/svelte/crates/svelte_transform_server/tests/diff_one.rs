//! Debug helper: diff server output for one specific fixture.

use std::fs;
use std::path::PathBuf;
use svelte_codegen_js::{default_visitors, print, PrintOptions};
use svelte_parse::parse;
use svelte_transform_server::server_component;

fn diff(name: &str, component: &str) {
    let base = PathBuf::from(format!(
        "../../../../packages/svelte/tests/snapshot/samples/{name}"
    ));
    let src = fs::read_to_string(base.join("index.svelte")).unwrap();
    let expected =
        fs::read_to_string(base.join("_expected/server/index.svelte.js")).unwrap();
    let root = parse(&src, false).unwrap();
    let prog = server_component(&root, component);
    let r = print(&prog, &default_visitors(), &PrintOptions::default());
    println!("=== EXPECTED ===\n{expected}\n=== GOT ===\n{}", r.code);
    if r.code != expected {
        panic!("diff for {name}");
    }
}

#[test]
#[ignore]
fn diff_nullish() {
    diff("nullish-coallescence-omittance", "Nullish_coallescence_omittance");
}

#[test]
#[ignore]
fn diff_async_top_level_inspect_server() {
    diff(
        "async-top-level-inspect-server",
        "Async_top_level_inspect_server",
    );
}

#[test]
#[ignore]
fn diff_bind_component_snippet() {
    diff("bind-component-snippet", "Bind_component_snippet");
}

#[test]
#[ignore]
fn diff_skip_static_subtree() {
    diff("skip-static-subtree", "Skip_static_subtree");
}

#[test]
#[ignore]
fn diff_async_const() {
    diff("async-const", "Async_const");
}

#[test]
#[ignore]
fn diff_async_if_chain() {
    diff("async-if-chain", "Async_if_chain");
}

#[test]
#[ignore]
fn diff_async_in_derived() {
    diff("async-in-derived", "Async_in_derived");
}

#[test]
#[ignore]
fn diff_async_top_level_group_sync_run() {
    diff(
        "async-top-level-group-sync-run",
        "Async_top_level_group_sync_run",
    );
}

#[test]
#[ignore]
fn diff_select_with_rich_content() {
    diff("select-with-rich-content", "Select_with_rich_content");
}
