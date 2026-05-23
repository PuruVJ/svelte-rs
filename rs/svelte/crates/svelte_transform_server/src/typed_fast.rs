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
use std::borrow::Cow;

/// Try to lower `root` end-to-end via the typed path. Returns `None` for
/// shapes that need the legacy/Value path (any script, dynamic content,
/// CSS scoping hooks, etc).
pub fn try_typed_server(root: &Root<'_>, component_name: &str) -> Option<Program> {
    try_typed_server_with(root, component_name, false)
}

pub fn try_typed_server_with(
    root: &Root<'_>,
    component_name: &str,
    experimental_async: bool,
) -> Option<Program> {
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
            name: Cow::Owned(component_name.to_string()),
            span: Span::ZERO,
        }),
        params: vec![t::pat_id("$$renderer")],
        param_type_annotations: Vec::new(),
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

    let mut prog: Vec<Statement> = Vec::with_capacity(3);
    if experimental_async {
        prog.push(t::import_side_effect("svelte/internal/flags/async"));
    }
    prog.push(import);
    prog.push(export);
    Some(t::program(prog))
}

/// Walk a fragment and return its content as one HTML string if and only
/// if every child is either static text or a void/simple element with no
/// dynamic attributes. Returns `None` on the first dynamic node OR on any
/// `<option>` (which upstream lowers to a `$$renderer.option(...)` call even
/// in static contexts — falls through to the dynamic path).
fn static_html(fragment: &Fragment<'_>) -> Option<String> {
    if fragment_contains_option_element(fragment) {
        return None;
    }
    // Trim leading + trailing pure-WS Text at the fragment boundary, then
    // append children. Matches upstream's clean_nodes whitespace trim.
    let mut start = 0;
    let mut end = fragment.nodes.len();
    while start < end {
        if matches!(&fragment.nodes[start], FragmentChild::Text(t) if t.data.trim().is_empty()) {
            start += 1;
        } else {
            break;
        }
    }
    while end > start {
        if matches!(&fragment.nodes[end - 1], FragmentChild::Text(t) if t.data.trim().is_empty()) {
            end -= 1;
        } else {
            break;
        }
    }
    // `is_text_first`: if the fragment's first non-WS child is text, the
    // top-level push needs a leading `<!---->` anchor (so the text doesn't
    // get fused into a neighbouring fragment during hydration).
    let needs_anchor = matches!(
        fragment.nodes.get(start),
        Some(FragmentChild::Text(_))
    );
    let mut out = String::with_capacity(64);
    if needs_anchor {
        out.push_str("<!---->");
    }
    for c in &fragment.nodes[start..end] {
        append_static(c, &mut out)?;
    }
    // Also trim any leading whitespace from the first text and trailing
    // whitespace from the last.
    let trimmed = out.trim_matches(|c: char| c == ' ').to_string();
    Some(trimmed)
}

/// True if any descendant of `fragment` is a `<select>` or `<option>` element —
/// those need the customizable-select-element call shape, not static HTML.
fn fragment_contains_option_element(fragment: &Fragment<'_>) -> bool {
    fragment.nodes.iter().any(node_contains_option_element)
}

fn node_contains_option_element(n: &FragmentChild<'_>) -> bool {
    match n {
        FragmentChild::RegularElement(el) => {
            el.name == "option"
                || el.name == "select"
                || fragment_contains_option_element(&el.fragment)
        }
        _ => false,
    }
}

fn append_static(child: &FragmentChild<'_>, out: &mut String) -> Option<()> {
    match child {
        FragmentChild::Text(t) => {
            // Collapse whitespace runs to a single space (matching upstream's
            // HTML-text collapse rules) and escape template-literal specials.
            let collapsed = collapse_ws(&t.data);
            for ch in collapsed.chars() {
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

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}

fn is_void(name: &str) -> bool {
    matches!(
        name,
        "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input"
            | "link" | "meta" | "param" | "source" | "track" | "wbr"
    )
}
