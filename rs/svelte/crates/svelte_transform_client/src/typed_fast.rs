//! Fully-typed `client_component` fast path for the simple-static
//! template case.
//!
//! Recognized shapes:
//! - No `<script>` / `<script context="module">`, no `<style>`.
//! - Static-only content (no `{expr}`, blocks, components, directives).
//! - Single OR multiple top-level non-whitespace elements.
//!
//! Output (single-root):
//! ```js
//! var root = $.from_html(`HTML`);
//! export default function Name($$anchor) {
//!     var TAG = root();
//!     $.append($$anchor, TAG);
//! }
//! ```
//!
//! Output (multi-root):
//! ```js
//! var root = $.from_html(`HTML`, 1);
//! export default function Name($$anchor) {
//!     var fragment = root();
//!     $.append($$anchor, fragment);
//! }
//! ```

use svelte_ast::root::Root;
use svelte_ast::fragment::{Fragment, FragmentChild};
use svelte_js_ast::*;
use svelte_transform_shared::builders_typed as t;

pub fn try_typed_client(root: &Root, component_name: &str) -> Option<Program> {
    if root.instance.is_some() || root.module.is_some() {
        return None;
    }
    if root.css.is_some() {
        return None;
    }
    let (root_var_name, html, multi_root_count) = static_root(&root.fragment)?;
    let is_multi_root = multi_root_count > 1;

    let import_disclose = t::import_side_effect("svelte/internal/disclose-version");
    let import_flags = t::import_side_effect("svelte/internal/flags/legacy");
    let import_internal = t::import_namespace("$", "svelte/internal/client");

    // `var root = $.from_html(\`HTML\`[, 1]);`
    let mut from_html_args = vec![t::template_raw(vec![html], vec![])];
    if is_multi_root {
        from_html_args.push(t::lit_number(1.0));
    }
    let root_var = t::var(
        "root",
        t::call(t::member_id(t::id("$"), "from_html"), from_html_args),
    );

    let inner_var = t::var(&root_var_name, t::call(t::id("root"), vec![]));
    let append_call = t::call(
        t::member_id(t::id("$"), "append"),
        vec![t::id("$$anchor"), t::id_owned(root_var_name.to_string())],
    );
    let mut func_body = vec![inner_var];
    // Multi-root templates need `$.next(2*(N-1))` to position the cursor
    // past the spacers between top-level elements before appending.
    // Mirrors upstream's behavior for static multi-root templates.
    if multi_root_count > 1 {
        let offset = (2 * (multi_root_count - 1)) as f64;
        func_body.push(t::stmt(t::call(
            t::member_id(t::id("$"), "next"),
            vec![t::lit_number(offset)],
        )));
    }
    func_body.push(t::stmt(append_call));

    let export = t::export_default_function(
        component_name,
        vec![t::pat_id("$$anchor")],
        func_body,
    );

    Some(t::program(vec![
        import_disclose,
        import_flags,
        import_internal,
        root_var,
        export,
    ]))
}

/// Returns `(var_name, html, top_count)` for a fully-static fragment.
/// `var_name` is the tag name for single-root templates, `"fragment"` for
/// multi-root. `top_count` is the number of top-level element-like roots.
fn static_root(fragment: &Fragment) -> Option<(String, String, usize)> {
    let non_ws: Vec<&FragmentChild> = fragment
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            _ => true,
        })
        .collect();
    if non_ws.is_empty() {
        return None;
    }

    // Single non-WS root: use the element's name as the variable.
    if non_ws.len() == 1 {
        if let FragmentChild::RegularElement(el) = non_ws[0] {
            // `<input>` with `checked` or `value` needs
            // `$.remove_input_defaults(input)` hydration → bail.
            if el.name == "input"
                && el.attributes.iter().any(|a| matches!(
                    a,
                    svelte_ast::attributes::ElementAttribute::Attribute(attr)
                        if attr.name == "checked" || attr.name == "value"
                ))
            {
                return None;
            }
            // `<select>` with rich `<option>` content needs the
            // customizable_select handler — defer to the walker.
            if el.name == "select"
                && el.fragment.nodes.iter().any(|n| matches!(
                    n,
                    FragmentChild::RegularElement(opt)
                        if opt.name == "option"
                            && opt.fragment.nodes.iter().any(|c| matches!(
                                c, FragmentChild::RegularElement(_)
                            ))
                ))
            {
                return None;
            }
            let mut html = String::with_capacity(32);
            html.push('<');
            html.push_str(&el.name);
            for attr in &el.attributes {
                if let svelte_ast::attributes::ElementAttribute::Attribute(a) = attr {
                    append_static_attr(a, &mut html)?;
                } else {
                    return None;
                }
            }
            if is_void(&el.name) {
                html.push_str("/>");
                return Some((sanitize_var(&el.name), html, 1));
            }
            html.push('>');
            for child in trim_boundary_whitespace(&el.fragment.nodes) {
                append_static(child, &mut html)?;
            }
            html.push_str("</");
            html.push_str(&el.name);
            html.push('>');
            return Some((sanitize_var(&el.name), html, 1));
        }
        return None;
    }

    // Multiple roots: walk the whole fragment, including whitespace between
    // top-level siblings (collapse runs to single spaces).
    let mut html = String::with_capacity(64);
    let trimmed = trim_boundary_whitespace(&fragment.nodes);
    for n in trimmed {
        match n {
            FragmentChild::Text(t) => {
                // Collapse whitespace runs to single space.
                let collapsed = collapse_ws(&t.data);
                for ch in collapsed.chars() {
                    match ch {
                        '`' => html.push_str("\\`"),
                        '\\' => html.push_str("\\\\"),
                        _ => html.push(ch),
                    }
                }
            }
            FragmentChild::RegularElement(el) => {
                // `<input>` with `checked` or `value` → bail (deep_static
                // handles `$.remove_input_defaults(input)`).
                if el.name == "input"
                    && el.attributes.iter().any(|a| matches!(
                        a,
                        svelte_ast::attributes::ElementAttribute::Attribute(attr)
                            if attr.name == "checked" || attr.name == "value"
                    ))
                {
                    return None;
                }
                if !el.attributes.is_empty() {
                    // Attributes — walk and only static-text-value ones supported.
                    html.push('<');
                    html.push_str(&el.name);
                    for attr in &el.attributes {
                        if let svelte_ast::attributes::ElementAttribute::Attribute(a) = attr {
                            if let Some(()) = append_static_attr(a, &mut html) {
                                continue;
                            }
                        }
                        return None;
                    }
                    if is_void(&el.name) {
                        html.push_str("/>");
                        continue;
                    }
                    html.push('>');
                } else {
                    html.push('<');
                    html.push_str(&el.name);
                    html.push('>');
                    if is_void(&el.name) {
                        continue;
                    }
                }
                let children = trim_boundary_whitespace(&el.fragment.nodes);
                for c in children {
                    append_static_to_string(c, &mut html)?;
                }
                html.push_str("</");
                html.push_str(&el.name);
                html.push('>');
            }
            _ => return None,
        }
    }
    // Count top-level element/text/expression roots that produce a node
    // — these define the sibling stride for `$.next(2*(N-1))`. We exclude
    // whitespace-only text since `trim_boundary_whitespace` collapses
    // those into separators, not standalone roots.
    let top_count = trimmed
        .iter()
        .filter(|n| match n {
            FragmentChild::RegularElement(_) => true,
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            _ => false,
        })
        .count();
    Some(("fragment".to_string(), html, top_count))
}

fn trim_boundary_whitespace(nodes: &[FragmentChild]) -> &[FragmentChild] {
    let mut start = 0;
    let mut end = nodes.len();
    while start < end {
        if matches!(&nodes[start], FragmentChild::Text(t) if t.data.trim().is_empty()) {
            start += 1;
        } else {
            break;
        }
    }
    while end > start {
        if matches!(&nodes[end - 1], FragmentChild::Text(t) if t.data.trim().is_empty()) {
            end -= 1;
        } else {
            break;
        }
    }
    &nodes[start..end]
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

fn append_static_attr(a: &svelte_ast::attributes::Attribute, html: &mut String) -> Option<()> {
    use svelte_ast::attributes::{AttributeValue, AttributeValuePart};
    // `dir` attribute needs `template_effect(() => el.dir = el.dir)`
    // (Chromium fix). Force typed_fast to bail.
    if a.name == "dir" {
        return None;
    }
    match &a.value {
        AttributeValue::Empty => {
            // Serialize bare attrs as `name=""` to match upstream/server.
            html.push(' ');
            html.push_str(&a.name);
            html.push_str("=\"\"");
            Some(())
        }
        AttributeValue::Single(_) => None,
        AttributeValue::Many(parts) => {
            if !parts.iter().all(|p| matches!(p, AttributeValuePart::Text(_))) {
                return None;
            }
            html.push(' ');
            html.push_str(&a.name);
            html.push_str("=\"");
            for p in parts {
                if let AttributeValuePart::Text(t) = p {
                    for c in t.data.chars() {
                        match c {
                            '"' => html.push_str("&quot;"),
                            '&' => html.push_str("&amp;"),
                            '`' => html.push_str("\\`"),
                            '\\' => html.push_str("\\\\"),
                            _ => html.push(c),
                        }
                    }
                }
            }
            html.push('"');
            Some(())
        }
    }
}

fn append_static(child: &FragmentChild, out: &mut String) -> Option<()> {
    append_static_to_string(child, out)
}

fn append_static_to_string(child: &FragmentChild, out: &mut String) -> Option<()> {
    match child {
        FragmentChild::Text(t) => {
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
                out.push('<');
                out.push_str(&el.name);
                for attr in &el.attributes {
                    if let svelte_ast::attributes::ElementAttribute::Attribute(a) = attr {
                        if let Some(()) = append_static_attr(a, out) {
                            continue;
                        }
                    }
                    return None;
                }
                if is_void(&el.name) {
                    out.push_str("/>");
                    return Some(());
                }
                out.push('>');
            } else {
                out.push('<');
                out.push_str(&el.name);
                out.push('>');
                if is_void(&el.name) {
                    return Some(());
                }
            }
            let children = trim_boundary_whitespace(&el.fragment.nodes);
            for c in children {
                append_static_to_string(c, out)?;
            }
            out.push('<');
            out.push('/');
            out.push_str(&el.name);
            out.push('>');
            Some(())
        }
        _ => None,
    }
}

fn sanitize_var(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect()
}

fn is_void(name: &str) -> bool {
    matches!(
        name,
        "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input"
            | "link" | "meta" | "param" | "source" | "track" | "wbr"
    )
}
