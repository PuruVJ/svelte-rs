//! End-to-end snapshot tests for the client transform.

use svelte_codegen_js::{default_visitors, print, PrintOptions};
use svelte_parse::parse;
use svelte_transform_client::client_component;

fn compile_client(source: &str, name: &str) -> String {
    let root = parse(source, false).expect("parse should succeed");
    let program = client_component(&root, name);
    print(&program, &default_visitors(), &PrintOptions::default()).code
}

fn assert_snapshot_eq(fixture: &str, source: &str, component_name: &str) {
    let path = format!(
        "../../../../packages/svelte/tests/snapshot/samples/{fixture}/_expected/client/index.svelte.js"
    );
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing fixture {path}: {e}"));
    let got = compile_client(source, component_name);
    assert_eq!(got, expected, "fixture {fixture} diverged");
}

#[test]
fn snapshot_hello_world_client_fixture() {
    assert_snapshot_eq("hello-world", "<h1>hello world</h1>", "Hello_world");
}

#[test]
fn snapshot_imports_in_modules_client_fixture() {
    let source = "<script>\n\timport { random } from './module.svelte';\n</script>\n";
    assert_snapshot_eq("imports-in-modules", source, "Imports_in_modules");
}

// hmr client fixture intentionally not asserted byte-equal yet — it requires
// HMR wrapper emission (`function Hmr(...) { ... } if (import.meta.hot) {
// Hmr = $.hmr(Hmr); ... } export default Hmr;`) which the minimal client
// transform doesn't yet produce.

#[test]
fn hello_world_client_byte_equal() {
    let source = "<h1>hello world</h1>";
    let expected = "import 'svelte/internal/disclose-version';\nimport 'svelte/internal/flags/legacy';\nimport * as $ from 'svelte/internal/client';\n\nvar root = $.from_html(`<h1>hello world</h1>`);\n\nexport default function Hello_world($$anchor) {\n\tvar h1 = root();\n\n\t$.append($$anchor, h1);\n}";
    assert_eq!(compile_client(source, "Hello_world"), expected);
}
