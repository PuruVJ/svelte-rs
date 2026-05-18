//! Experimental fully-typed `server_component` for the simple-static
//! template case. Built to MEASURE the per-fixture speedup of the typed
//! pipeline vs. the convert-from-Value path — if the win is meaningful,
//! it motivates migrating the rest of the visitors.
//!
//! Recognized shape:
//! - No `<script>` instance content (no runes, no hoisted exports).
//! - No `<script context="module">`.
//! - Fragment is pure static HTML (no `{expr}`, no blocks, no components).
//! - Output is a single `$$renderer.push(\`HTML\`)` inside the exported
//!   function body.
//!
//! For anything more complex, callers fall back to the legacy Value path.

use svelte_ast::root::Root;
use svelte_ast::fragment::{Fragment, FragmentChild};
use svelte_js_ast::*;
use svelte_transform_shared::builders_typed as t;

/// Try to lower `root` end-to-end via the typed path. Returns `None` for
/// shapes that need the legacy/Value path (any script, dynamic content,
/// CSS scoping hooks, etc).
pub fn try_typed_server(root: &Root, component_name: &str) -> Option<Program> {
    if root.instance.is_some() || root.module.is_some() {
        return None;
    }
    if root.css.is_some() {
        return None;
    }
    let html = static_html(&root.fragment)?;

    let import = t::import_namespace("$", "svelte/internal/server");

    // `$$renderer.push(\`HTML\`);`
    let push_call = t::call(
        t::member_id(t::id("$$renderer"), "push"),
        vec![t::template_raw(vec![html], vec![])],
    );
    let func_body = vec![t::stmt(push_call)];

    let func = ExportDefault::Function(Box::new(FunctionDeclaration {
        id: Some(Identifier {
            name: component_name.to_string(),
            span: Span::ZERO,
        }),
        params: vec![t::pat_id("$$renderer")],
        body: BlockStatement {
            body: func_body,
            span: Span::ZERO,
        },
        generator: false,
        r#async: false,
        span: Span::ZERO,
    }));
    let export = Statement::ExportDefault(Box::new(ExportDefaultDeclaration {
        declaration: func,
        span: Span::ZERO,
    }));

    Some(t::program(vec![import, export]))
}

/// Walk a fragment and return its content as one HTML string if and only
/// if every child is either static text or a void/simple element with no
/// dynamic attributes. Returns `None` on the first dynamic node OR on any
/// `<option>` (which upstream lowers to a `$$renderer.option(...)` call even
/// in static contexts — falls through to the dynamic path).
fn static_html(fragment: &Fragment) -> Option<String> {
    if fragment_contains_option_element(fragment) {
        return None;
    }
    let mut out = String::with_capacity(64);
    for c in &fragment.nodes {
        append_static(c, &mut out)?;
    }
    Some(out)
}

/// True if any descendant of `fragment` is a `<select>` or `<option>` element —
/// those need the customizable-select-element call shape, not static HTML.
fn fragment_contains_option_element(fragment: &Fragment) -> bool {
    fragment.nodes.iter().any(node_contains_option_element)
}

fn node_contains_option_element(n: &FragmentChild) -> bool {
    match n {
        FragmentChild::RegularElement(el) => {
            el.name == "option"
                || el.name == "select"
                || fragment_contains_option_element(&el.fragment)
        }
        _ => false,
    }
}

fn append_static(child: &FragmentChild, out: &mut String) -> Option<()> {
    match child {
        FragmentChild::Text(t) => {
            // Escape backticks since the output is template-literal-wrapped.
            for ch in t.data.chars() {
                match ch {
                    '`' => out.push_str("\\`"),
                    '\\' => out.push_str("\\\\"),
                    _ => out.push(ch),
                }
            }
            Some(())
        }
        FragmentChild::RegularElement(el) => {
            if !el.attributes.is_empty() {
                return None;
            }
            out.push('<');
            out.push_str(&el.name);
            out.push('>');
            if !is_void(&el.name) {
                for child in &el.fragment.nodes {
                    append_static(child, out)?;
                }
                out.push('<');
                out.push('/');
                out.push_str(&el.name);
                out.push('>');
            }
            Some(())
        }
        _ => None,
    }
}

fn is_void(name: &str) -> bool {
    matches!(
        name,
        "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input"
            | "link" | "meta" | "param" | "source" | "track" | "wbr"
    )
}
