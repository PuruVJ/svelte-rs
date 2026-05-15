//! Client-side template serialization.
//!
//! Walks a Svelte template `Fragment` and produces the static HTML string
//! that `$.from_html(\`...\`)` consumes at runtime. The template is the
//! skeleton: dynamic interpolations and bindings become separate update
//! calls (TODO — for now we only handle pure-static templates).
//!
//! Mirrors `phases/3-transform/client/visitors/{Fragment, RegularElement,
//! TitleElement, ...}.js`.

use svelte_ast::attributes::{Attribute, AttributeValue, AttributeValuePart, ElementAttribute};
use svelte_ast::elements::RegularElement;
use svelte_ast::fragment::{Fragment, FragmentChild};

/// Build the static HTML skeleton for a fragment. Returns the raw HTML
/// suitable for embedding inside a `$.from_html(\`...\`)` template literal.
pub fn serialize_static_html(fragment: &Fragment) -> String {
    let mut out = String::new();
    for child in &fragment.nodes {
        serialize_child(child, &mut out);
    }
    out
}

fn serialize_child(child: &FragmentChild, out: &mut String) {
    match child {
        FragmentChild::Text(t) => out.push_str(&t.data),
        FragmentChild::RegularElement(el) => serialize_regular_element(el, out),
        // Expressions / blocks / components produce placeholders in the
        // template; for now we drop them (the simple-static cases only).
        _ => {}
    }
}

fn serialize_regular_element(el: &RegularElement, out: &mut String) {
    out.push('<');
    out.push_str(&el.name);
    for attr in &el.attributes {
        if let Some(s) = serialize_static_attribute(attr) {
            out.push_str(&s);
        }
    }
    if is_void_element(&el.name) {
        out.push('>');
        return;
    }
    out.push('>');
    for child in &el.fragment.nodes {
        serialize_child(child, out);
    }
    out.push_str("</");
    out.push_str(&el.name);
    out.push('>');
}

fn serialize_static_attribute(attr: &ElementAttribute) -> Option<String> {
    let ElementAttribute::Attribute(Attribute { name, value, .. }) = attr else {
        return None;
    };
    match value {
        AttributeValue::Empty(true) => Some(format!(" {name}")),
        AttributeValue::Empty(false) => Some(String::new()),
        AttributeValue::Many(parts) => {
            let mut text = String::new();
            for p in parts {
                match p {
                    AttributeValuePart::Text(t) => text.push_str(&t.data),
                    AttributeValuePart::ExpressionTag(_) => return None,
                }
            }
            Some(format!(" {name}=\"{}\"", escape(&text)))
        }
        AttributeValue::Single(_) => None,
    }
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("&quot;"),
            '&' => out.push_str("&amp;"),
            _ => out.push(ch),
        }
    }
    out
}

fn is_void_element(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}
