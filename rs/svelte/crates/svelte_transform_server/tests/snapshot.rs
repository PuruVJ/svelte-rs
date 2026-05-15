//! End-to-end snapshot tests for the server transform.
//!
//! Each test parses a `.svelte` source string, runs the server transform,
//! prints with `svelte_codegen_js`, and asserts byte-equality with the
//! corresponding fixture in `packages/svelte/tests/snapshot/samples/*/_expected/server/index.svelte.js`.

use svelte_codegen_js::{default_visitors, print, PrintOptions};
use svelte_parse::parse;
use svelte_transform_server::server_component;

fn compile_server(source: &str, name: &str) -> String {
    let root = parse(source, false).expect("parse should succeed");
    let program = server_component(&root, name);
    print(&program, &default_visitors(), &PrintOptions::default()).code
}

#[test]
fn hello_world_server() {
    let source = "<h1>hello world</h1>";
    let expected = "import * as $ from 'svelte/internal/server';\n\nexport default function Hello_world($$renderer) {\n\t$$renderer.push(`<h1>hello world</h1>`);\n}";
    assert_eq!(compile_server(source, "Hello_world"), expected);
}

#[test]
fn hmr_server() {
    let source = "<h1>hello world</h1>";
    let expected = "import * as $ from 'svelte/internal/server';\n\nexport default function Hmr($$renderer) {\n\t$$renderer.push(`<h1>hello world</h1>`);\n}";
    assert_eq!(compile_server(source, "Hmr"), expected);
}

#[test]
fn nested_elements_server() {
    let source = "<div><span>hi</span></div>";
    let out = compile_server(source, "Nested");
    assert!(out.contains("`<div><span>hi</span></div>`"));
}

#[test]
fn void_element_self_closing_in_html() {
    let source = "<br/>";
    let out = compile_server(source, "Void_test");
    // Void elements emit as `<br>` not `<br/>` per HTML serialization rules.
    assert!(out.contains("`<br>`"), "got: {out}");
}

#[test]
fn static_attribute_serializes() {
    let source = r#"<div class="foo bar">hi</div>"#;
    let out = compile_server(source, "Attr");
    assert!(out.contains(r#"`<div class="foo bar">hi</div>`"#), "got: {out}");
}

#[test]
fn bare_attribute_serializes() {
    let source = "<input disabled>";
    let out = compile_server(source, "Bare");
    assert!(out.contains("`<input disabled>`"), "got: {out}");
}

#[test]
fn attribute_quoting_escapes_quotes() {
    let source = r#"<div title='he said "hi"'>x</div>"#;
    let out = compile_server(source, "Esc");
    assert!(out.contains(r#"&quot;hi&quot;"#), "got: {out}");
}

#[test]
fn expression_tag_wraps_in_escape() {
    let source = "<p>{name}</p>";
    let out = compile_server(source, "Expr");
    // ExpressionTag `{name}` should emit as `<p>${$.escape(name)}</p>`.
    assert!(out.contains("`<p>${$.escape(name)}</p>`"), "got: {out}");
}

#[test]
fn html_tag_wraps_in_html_helper() {
    let source = "<div>{@html content}</div>";
    let out = compile_server(source, "Raw");
    // `{@html content}` should emit as `<div>${$.html(content)}</div>`.
    assert!(out.contains("`<div>${$.html(content)}</div>`"), "got: {out}");
}

#[test]
fn dynamic_attribute_uses_attr_helper() {
    let source = "<div class={cls}>x</div>";
    let out = compile_server(source, "DynAttr");
    // Single-value dynamic attribute → `$.attr('name', value)` runtime helper.
    assert!(
        out.contains(r#"`<div${$.attr('class', cls)}>x</div>`"#),
        "got: {out}"
    );
}

#[test]
fn if_block_lowers_to_if_statement() {
    let source = "{#if cond}<p>yes</p>{:else}<p>no</p>{/if}";
    let out = compile_server(source, "Conditional");
    assert!(out.contains("$$renderer.push(`<!--[-->`)"), "got: {out}");
    assert!(out.contains("if (cond)"), "got: {out}");
    assert!(out.contains("$$renderer.push(`<p>yes</p>`)"), "got: {out}");
    assert!(out.contains("$$renderer.push(`<p>no</p>`)"), "got: {out}");
    assert!(out.contains("$$renderer.push(`<!--]-->`)"), "got: {out}");
}

#[test]
fn if_block_without_else() {
    let source = "{#if cond}<p>hi</p>{/if}";
    let out = compile_server(source, "Cond2");
    assert!(out.contains("if (cond)"), "got: {out}");
    assert!(!out.contains(" else "), "should not emit else: {out}");
}

#[test]
fn each_block_lowers_to_for_loop() {
    let source = "{#each items as item}<li>{item}</li>{/each}";
    let out = compile_server(source, "Listing");
    assert!(out.contains("const each_array = $.ensure_array_like(items);"), "got: {out}");
    assert!(out.contains("for (let $$index = 0, $$length = each_array.length;"), "got: {out}");
    assert!(out.contains("let item = each_array[$$index];"), "got: {out}");
    assert!(out.contains("$.escape(item)"), "got: {out}");
}

#[test]
fn each_block_with_index() {
    let source = "{#each xs as x, i}<p>{i}: {x}</p>{/each}";
    let out = compile_server(source, "Indexed");
    assert!(out.contains("const i = $$index;"), "got: {out}");
}

#[test]
fn component_call_with_props() {
    let source = "<Foo a={x} b='hello' />";
    let out = compile_server(source, "WithComp");
    assert!(out.contains("Foo($$renderer, { a: x, b: 'hello' });"), "got: {out}");
}

#[test]
fn component_call_no_props() {
    let source = "<Foo />";
    let out = compile_server(source, "NoProps");
    assert!(out.contains("Foo($$renderer, {});"), "got: {out}");
}

#[test]
fn component_with_spread_attribute() {
    let source = "<Foo {...rest} />";
    let out = compile_server(source, "Spread");
    assert!(out.contains("Foo($$renderer, { ...rest });"), "got: {out}");
}

#[test]
fn key_block_renders_body() {
    let source = "{#key x}<p>hi</p>{/key}";
    let out = compile_server(source, "K");
    assert!(out.contains("`<!--[--><p>hi</p><!--]-->`"), "got: {out}");
}

#[test]
fn await_block_renders_try_catch() {
    let source = "{#await p then v}<p>{v}</p>{:catch e}<p>err</p>{/await}";
    let out = compile_server(source, "Awaiting");
    assert!(out.contains("try {"), "got: {out}");
    assert!(out.contains("const v = await p"), "got: {out}");
    assert!(out.contains("catch (e)"), "got: {out}");
}

#[test]
fn snippet_block_hoists_to_function() {
    let source = "{#snippet item(name)}<li>{name}</li>{/snippet}";
    let out = compile_server(source, "Sn");
    assert!(out.contains("function item($$renderer, name)"), "got: {out}");
}

#[test]
fn const_tag_emits_declaration() {
    let source = "{#each xs as x}{@const doubled = x * 2}<p>{doubled}</p>{/each}";
    let out = compile_server(source, "Const");
    assert!(out.contains("const doubled = x * 2;"), "got: {out}");
}

#[test]
fn render_tag_invokes_snippet_with_renderer() {
    let source = "{@render item('foo')}";
    let out = compile_server(source, "Render");
    assert!(out.contains("item($$renderer, 'foo');"), "got: {out}");
}

#[test]
fn debug_tag_emits_nothing() {
    let source = "<p>{@debug x}</p>";
    let out = compile_server(source, "Debug");
    // No debugger statement should appear in output; just the wrapping <p></p>.
    assert!(!out.contains("debugger"), "got: {out}");
}

#[test]
fn instance_script_imports_hoisted_to_top() {
    let source = "<script>\nimport foo from './foo';\nlet x = 1;\n</script>\n<p>{x}</p>";
    let out = compile_server(source, "Withscript");
    // Import should appear above export default
    let import_pos = out.find("import foo from './foo'").expect("import emitted");
    let export_pos = out.find("export default").expect("export default emitted");
    assert!(
        import_pos < export_pos,
        "import not above export default: {out}"
    );
    // let x = 1 should be inside the function body
    assert!(out.contains("let x = 1;"), "let-decl missing: {out}");
}

#[test]
fn module_script_body_at_top() {
    let source = "<script module>\nexport const meta = 'foo';\n</script>\n<p>hi</p>";
    let out = compile_server(source, "WithMod");
    assert!(out.contains("meta"), "got: {out}");
}

#[test]
fn each_string_template_marker_inserted() {
    // Each-block body with dynamic content gets a `<!---->` marker prepended.
    let source = "{#each xs as x}{x}{/each}";
    let out = compile_server(source, "Em");
    assert!(out.contains("`<!---->${$.escape(x)}`"), "got: {out}");
}

#[test]
fn each_block_whitespace_trimmed() {
    // Leading/trailing whitespace inside each-body is stripped.
    let source = "{#each xs as x}\n\t{x}\n{/each}";
    let out = compile_server(source, "Etw");
    assert!(out.contains("`<!---->${$.escape(x)}`"), "got: {out}");
    // The "\n\t" / "\n" outer text should not appear inside the template.
    assert!(!out.contains("\\n\\t"), "got: {out}");
}

#[test]
fn bind_this_byte_equal() {
    let source = "<Foo bind:this={foo} />";
    let expected = "import * as $ from 'svelte/internal/server';\n\nexport default function Bind_this($$renderer) {\n\tFoo($$renderer, {});\n}";
    assert_eq!(compile_server(source, "Bind_this"), expected);
}

/// Read the bundled JS snapshot fixture and assert byte-equality. Fixture
/// path is relative to the crate root.
fn assert_snapshot_eq(fixture: &str, source: &str, component_name: &str) {
    let path = format!(
        "../../../../packages/svelte/tests/snapshot/samples/{fixture}/_expected/server/index.svelte.js"
    );
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing fixture {path}: {e}"));
    let got = compile_server(source, component_name);
    assert_eq!(got, expected, "fixture {fixture} diverged");
}

#[test]
fn snapshot_hello_world_fixture() {
    assert_snapshot_eq("hello-world", "<h1>hello world</h1>", "Hello_world");
}

#[test]
fn snapshot_hmr_fixture() {
    assert_snapshot_eq("hmr", "<h1>hello world</h1>", "Hmr");
}

#[test]
fn snapshot_bind_this_fixture() {
    assert_snapshot_eq("bind-this", "<Foo bind:this={foo} />", "Bind_this");
}

#[test]
fn snapshot_each_index_non_null_fixture() {
    let source = "{#each Array(10), i}\n\t<p>index: {i}</p>\n{/each}\n";
    assert_snapshot_eq("each-index-non-null", source, "Each_index_non_null");
}

#[test]
fn runes_lowered_server_side() {
    let source = "<script>\n\tlet x = $state(1);\n\tlet y = $derived(x * 2);\n\t$effect(() => console.log(x));\n</script>\n<p>{x}/{y}</p>";
    let out = compile_server(source, "Runes");
    // $state(1) → 1
    assert!(out.contains("let x = 1;"), "got: {out}");
    // $derived(x * 2) → $.derived(() => x * 2)
    assert!(out.contains("$.derived(() => x * 2)"), "got: {out}");
    // $effect(...) → undefined as a side-effect statement
    assert!(!out.contains("$effect"), "got: {out}");
    // Template still references x and y via $.escape
    assert!(out.contains("$.escape(x)"), "got: {out}");
}

#[test]
fn snapshot_imports_in_modules_fixture() {
    let source = "<script>\n\timport { random } from './module.svelte';\n</script>\n";
    assert_snapshot_eq("imports-in-modules", source, "Imports_in_modules");
}

#[test]
fn snapshot_each_string_template_fixture() {
    let source = "{#each ['foo', 'bar', 'baz'] as thing}\n\t{thing},{' '}\n{/each}\n";
    assert_snapshot_eq(
        "each-string-template",
        source,
        "Each_string_template",
    );
}

#[test]
fn delegated_locally_declared_shadowed_fixture() {
    // Reads the fixture from disk and compiles it. Verifies the server output
    // drops the `onclick={...}` event-listener directive and emits
    // `${$.attr('data-index', index)}` for the dynamic attribute.
    let path = "../../../../packages/svelte/tests/snapshot/samples/delegated-locally-declared-shadowed/index.svelte";
    let source = std::fs::read_to_string(path).expect("fixture present");
    let got = compile_server(&source, "Delegated_locally_declared_shadowed");
    assert!(
        got.contains(r#"$.attr('data-index', index)"#),
        "expected $.attr() for data-index, got:\n{got}"
    );
    // Event-listener directive should be dropped.
    assert!(!got.contains("onclick"), "onclick should not appear in server output:\n{got}");
}

#[test]
fn const_string_expression_inlines() {
    // `{'literal'}` constant-folds to literal text.
    let source = "<p>{'hello'}</p>";
    let out = compile_server(source, "Cs");
    assert!(out.contains("`<p>hello</p>`"), "got: {out}");
}
