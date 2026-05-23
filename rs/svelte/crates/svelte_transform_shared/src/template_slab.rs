//! Pre-serialized template HTML for static regions (foundation for parse-time slab).

use svelte_ast::fragment::{Fragment, FragmentChild};

/// Contiguous HTML for a template region plus hole markers for dynamic nodes.
#[derive(Debug, Clone, Default)]
pub struct TemplateSlab {
    pub html: String,
    pub needs_import_node: bool,
}

/// Serialize a fully-static fragment subtree into a slab (no `{expr}` / blocks).
pub fn try_static_fragment_slab(fragment: &Fragment) -> Option<TemplateSlab> {
    if fragment.metadata.dynamic {
        return None;
    }
    let mut slab = TemplateSlab::default();
    for n in &fragment.nodes {
        match n {
            FragmentChild::Text(t) => slab.html.push_str(&t.data),
            FragmentChild::Comment(_) => {}
            FragmentChild::RegularElement(el) if el.metadata.is_static_element => {
                push_static_element_html(el, &mut slab.html, &mut slab.needs_import_node);
            }
            _ => return None,
        }
    }
    Some(slab)
}

fn push_static_element_html(
    el: &svelte_ast::elements::RegularElement,
    out: &mut String,
    needs_import_node: &mut bool,
) {
    if el.name.contains('-') || el.name == "video" {
        *needs_import_node = true;
    }
    out.push('<');
    out.push_str(&el.name);
    for a in &el.attributes {
        if let svelte_ast::attributes::ElementAttribute::Attribute(attr) = a {
            out.push(' ');
            out.push_str(&attr.name);
            if let svelte_ast::attributes::AttributeValue::Many(parts) = &attr.value {
                out.push_str("=\"");
                for p in parts {
                    if let svelte_ast::attributes::AttributeValuePart::Text(t) = p {
                        out.push_str(&t.data);
                    }
                }
                out.push('"');
            } else {
                out.push_str("=\"\"");
            }
        }
    }
    if matches!(
        el.name,
        "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input" | "link" | "meta"
            | "param" | "source" | "track" | "wbr"
    ) {
        out.push_str("/>");
        return;
    }
    out.push('>');
    if let Some(child_slab) = try_static_fragment_slab(&el.fragment) {
        out.push_str(&child_slab.html);
        *needs_import_node |= child_slab.needs_import_node;
    }
    out.push_str("</");
    out.push_str(&el.name);
    out.push('>');
}
