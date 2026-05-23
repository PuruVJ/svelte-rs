//! Pre-serialize static element subtrees once per compile (avoids repeated walks in sparse emit).

use svelte_ast::attributes::ElementAttribute;
use svelte_ast::fragment::{FragmentChild, Fragment};
use svelte_ast::root::Root;

use crate::walker::serialize_element_to_html_inner;

/// Fill `element.metadata.cached_static_html` for every static element in the tree.
pub fn precompute_static_html_cache(root: &mut Root<'_>) {
    precompute_fragment(&mut root.fragment);
}

fn precompute_fragment(fragment: &mut Fragment<'_>) {
    for node in &mut fragment.nodes {
        precompute_node(node);
    }
}

fn precompute_node(node: &mut FragmentChild<'_>) {
    match node {
        FragmentChild::RegularElement(el) => {
            if el.metadata.is_static_element && el.metadata.cached_static_html.is_none() {
                let mut html = String::new();
                let mut needs_import_node = false;
                if serialize_element_to_html_inner(el, &mut html, &mut needs_import_node).is_some()
                {
                    el.metadata.cached_static_html = Some(html);
                }
            }
            precompute_fragment(&mut el.fragment);
        }
        FragmentChild::Component(c) => precompute_fragment(&mut c.fragment),
        FragmentChild::SlotElement(el) => precompute_fragment(&mut el.fragment),
        FragmentChild::TitleElement(el) => precompute_fragment(&mut el.fragment),
        FragmentChild::SvelteElement(el) => precompute_fragment(&mut el.fragment),
        FragmentChild::SvelteComponent(c) => precompute_fragment(&mut c.fragment),
        FragmentChild::SvelteBody(el)
        | FragmentChild::SvelteBoundary(el)
        | FragmentChild::SvelteDocument(el)
        | FragmentChild::SvelteFragment(el)
        | FragmentChild::SvelteHead(el)
        | FragmentChild::SvelteWindow(el)
        | FragmentChild::SvelteSelf(el)
        | FragmentChild::SvelteOptions(el) => precompute_fragment(&mut el.fragment),
        FragmentChild::IfBlock(b) => {
            precompute_fragment(&mut b.consequent);
            if let Some(a) = &mut b.alternate {
                precompute_fragment(a);
            }
        }
        FragmentChild::EachBlock(b) => {
            precompute_fragment(&mut b.body);
            if let Some(f) = &mut b.fallback {
                precompute_fragment(f);
            }
        }
        FragmentChild::KeyBlock(b) => precompute_fragment(&mut b.fragment),
        FragmentChild::AwaitBlock(b) => {
            if let Some(p) = &mut b.pending {
                precompute_fragment(p);
            }
            if let Some(t) = &mut b.then {
                precompute_fragment(t);
            }
            if let Some(c) = &mut b.catch_ {
                precompute_fragment(c);
            }
        }
        FragmentChild::SnippetBlock(b) => precompute_fragment(&mut b.body),
        _ => {}
    }
}

/// Push cached or freshly serialized element HTML.
pub fn push_element_html(
    el: &svelte_ast::elements::RegularElement<'_>,
    out: &mut String,
    needs_import_node: &mut bool,
) -> Option<()> {
    if let Some(cached) = &el.metadata.cached_static_html {
        if el.name.contains('-') || el.name == "video" {
            *needs_import_node = true;
        }
        out.push_str(cached);
        return Some(());
    }
    serialize_element_to_html_inner(el, out, needs_import_node)
}
