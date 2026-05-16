//! Debug helper: diff client output for one specific fixture.

use std::fs;
use std::path::PathBuf;
use svelte_codegen_js::{default_visitors, print, PrintOptions};
use svelte_parse::parse;
use svelte_transform_client::{client_component_with_options, ClientOptions, FragmentsMode};

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
    if config.contains("fragments: 'tree'") || config.contains("fragments:'tree'") {
        options.fragments = FragmentsMode::Tree;
    }
    if config.contains("async: true") || config.contains("async:true") {
        options.experimental_async = true;
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

#[test]
#[ignore]
fn diff_function_prop_no_getter() {
    diff("function-prop-no-getter", "Function_prop_no_getter");
}

#[test]
#[ignore]
fn diff_text_nodes_deriveds() {
    diff("text-nodes-deriveds", "Text_nodes_deriveds");
}

#[test]
#[ignore]
fn diff_state_proxy_literal() {
    diff("state-proxy-literal", "State_proxy_literal");
}

#[test]
#[ignore]
fn diff_class_state() {
    diff(
        "class-state-field-constructor-assignment",
        "Class_state_field_constructor_assignment",
    );
}

#[test]
#[ignore]
fn diff_props_identifier() {
    diff("props-identifier", "Props_identifier");
}

#[test]
#[ignore]
fn diff_nullish() {
    diff("nullish-coallescence-omittance", "Nullish_coallescence_omittance");
}

#[test]
#[ignore]
fn diff_bind_component_snippet() {
    diff("bind-component-snippet", "Bind_component_snippet");
}

#[test]
#[ignore]
fn diff_await_block_scope() {
    diff("await-block-scope", "Await_block_scope");
}

#[test]
#[ignore]
fn diff_functional_templating() {
    diff("functional-templating", "Functional_templating");
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
fn diff_async_top_level_group_sync_run() {
    diff(
        "async-top-level-group-sync-run",
        "Async_top_level_group_sync_run",
    );
}
