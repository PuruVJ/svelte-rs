//! Debug helper: diff client output for one specific fixture.

use std::fs;
use std::path::PathBuf;
use svelte_codegen_js::{default_visitors, print, PrintOptions};
use svelte_parse::parse;
use svelte_transform_client::{client_component_with_options, ClientOptions};

#[test]
#[ignore]
fn diff_svelte_element() {
    let base = PathBuf::from(
        "../../../../packages/svelte/tests/snapshot/samples/svelte-element",
    );
    let src = fs::read_to_string(base.join("index.svelte")).unwrap();
    let expected =
        fs::read_to_string(base.join("_expected/client/index.svelte.js")).unwrap();
    let root = parse(&src, false).unwrap();
    let prog = client_component_with_options(&root, "Svelte_element", &ClientOptions::default());
    let r = print(&prog, &default_visitors(), &PrintOptions::default());
    println!("=== EXPECTED ===\n{expected}\n=== GOT ===\n{}", r.code);
    if r.code != expected {
        panic!("diff");
    }
}
