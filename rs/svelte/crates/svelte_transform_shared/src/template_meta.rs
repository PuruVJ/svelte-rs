//! Single-pass template metadata (mirrors upstream analyze `metadata.dynamic`).

use svelte_ast::attributes::ElementAttribute;
use svelte_ast::fragment::{Fragment, FragmentChild};
use svelte_ast::root::Root;

/// Mark `fragment.metadata.dynamic` / `element.metadata` for the whole component.
pub fn mark_template_metadata<'a>(root: &mut Root<'a>) {
    mark_fragment_dynamic(&mut root.fragment);
}

fn mark_fragment_dynamic(fragment: &mut Fragment) {
    let mut dynamic = false;
    for node in &mut fragment.nodes {
        if mark_node_dynamic(node) {
            dynamic = true;
        }
    }
    fragment.metadata.dynamic = dynamic;
}

fn mark_node_dynamic(node: &mut FragmentChild) -> bool {
    match node {
        FragmentChild::Text(_) | FragmentChild::Comment(_) => false,
        FragmentChild::ExpressionTag(_) | FragmentChild::HtmlTag(_) => true,
        FragmentChild::AttachTag(_)
        | FragmentChild::ConstTag(_)
        | FragmentChild::DebugTag(_)
        | FragmentChild::RenderTag(_) => true,
        FragmentChild::Component(c) => {
            c.metadata.dynamic = true;
            mark_fragment_dynamic(&mut c.fragment);
            true
        }
        FragmentChild::RegularElement(el) => mark_element_dynamic(el),
        FragmentChild::SlotElement(el) => {
            mark_fragment_dynamic(&mut el.fragment);
            true
        }
        FragmentChild::TitleElement(el) => {
            mark_fragment_dynamic(&mut el.fragment);
            true
        }
        FragmentChild::SvelteElement(el) => {
            mark_fragment_dynamic(&mut el.fragment);
            true
        }
        FragmentChild::SvelteComponent(c) => {
            mark_fragment_dynamic(&mut c.fragment);
            true
        }
        FragmentChild::SvelteBody(el)
        | FragmentChild::SvelteBoundary(el)
        | FragmentChild::SvelteDocument(el)
        | FragmentChild::SvelteFragment(el)
        | FragmentChild::SvelteHead(el)
        | FragmentChild::SvelteWindow(el)
        | FragmentChild::SvelteSelf(el)
        | FragmentChild::SvelteOptions(el) => {
            mark_fragment_dynamic(&mut el.fragment);
            true
        }
        FragmentChild::AwaitBlock(b) => {
            if let Some(p) = &mut b.pending {
                mark_fragment_dynamic(p);
            }
            if let Some(t) = &mut b.then {
                mark_fragment_dynamic(t);
            }
            if let Some(c) = &mut b.catch_ {
                mark_fragment_dynamic(c);
            }
            true
        }
        FragmentChild::EachBlock(b) => {
            mark_fragment_dynamic(&mut b.body);
            if let Some(f) = &mut b.fallback {
                mark_fragment_dynamic(f);
            }
            true
        }
        FragmentChild::IfBlock(b) => {
            mark_fragment_dynamic(&mut b.consequent);
            if let Some(a) = &mut b.alternate {
                mark_fragment_dynamic(a);
            }
            true
        }
        FragmentChild::KeyBlock(b) => {
            mark_fragment_dynamic(&mut b.fragment);
            true
        }
        FragmentChild::SnippetBlock(b) => {
            mark_fragment_dynamic(&mut b.body);
            true
        }
    }
}

fn mark_element_dynamic(el: &mut svelte_ast::elements::RegularElement) -> bool {
    let attr_dynamic = element_has_dynamic_attr(el);
    mark_fragment_dynamic(&mut el.fragment);
    let dynamic = attr_dynamic || el.fragment.metadata.dynamic;
    el.metadata.dynamic = dynamic;
    el.metadata.is_static_element = !dynamic && is_static_element_shape(el);
    dynamic
}

fn element_has_dynamic_attr(el: &svelte_ast::elements::RegularElement) -> bool {
    if el.name.contains('-') {
        return true;
    }
    for a in &el.attributes {
        match a {
            ElementAttribute::Attribute(attr) => {
                if matches!(attr.name, "autofocus" | "dir") {
                    return true;
                }
                if el.name == "input" && matches!(attr.name, "checked" | "value") {
                    return true;
                }
                if el.name == "source" || el.name == "video" || el.name == "audio" {
                    if attr.name == "muted" {
                        return true;
                    }
                }
                if el.name == "option" && attr.name == "value" {
                    return true;
                }
                match &attr.value {
                    svelte_ast::attributes::AttributeValue::Single(_) => return true,
                    svelte_ast::attributes::AttributeValue::Many(parts) => {
                        if !parts.iter().all(|p| {
                            matches!(p, svelte_ast::attributes::AttributeValuePart::Text(_))
                        }) {
                            return true;
                        }
                    }
                    svelte_ast::attributes::AttributeValue::Empty => {}
                }
            }
            _ => return true,
        }
    }
    false
}

fn is_static_element_shape(el: &svelte_ast::elements::RegularElement) -> bool {
    if matches!(el.name, "select" | "textarea" | "option") {
        return false;
    }
    for n in &el.fragment.nodes {
        match n {
            FragmentChild::Text(_) | FragmentChild::Comment(_) => {}
            FragmentChild::RegularElement(child) => {
                if !is_static_element_shape(child) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}
