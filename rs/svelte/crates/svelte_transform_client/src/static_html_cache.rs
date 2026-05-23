//! Pre-serialize static element subtrees once per compile (avoids repeated walks in sparse emit).

use svelte_ast::attributes::ElementAttribute;
use svelte_ast::fragment::{FragmentChild, Fragment};
use svelte_ast::root::Root;

use crate::walker::serialize_element_to_html_inner;

/// Fill `element.metadata.cached_static_html` for every static element in the tree.
pub fn precompute_static_html_cache<'a>(root: &mut Root<'a>, bump: &'a bumpalo::Bump) {
    precompute_fragment(&mut root.fragment, bump);
}

fn precompute_fragment<'a>(fragment: &mut Fragment<'a>, bump: &'a bumpalo::Bump) {
    for node in &mut fragment.nodes {
        precompute_node(node, bump);
    }
}

fn precompute_node<'a>(node: &mut FragmentChild<'a>, bump: &'a bumpalo::Bump) {
    match node {
        FragmentChild::RegularElement(el) => {
            if el.metadata.is_static_element && el.metadata.cached_static_html.is_none() {
                let mut html = String::new();
                let mut needs_import_node = false;
                if serialize_element_to_html_inner(el, &mut html, &mut needs_import_node).is_some()
                {
                    el.metadata.cached_static_html =
                        Some(bumpalo::collections::String::from_str_in(&html, bump));
                }
            }
            precompute_fragment(&mut el.fragment, bump);
        }
        FragmentChild::Component(c) => precompute_fragment(&mut c.fragment, bump),
        FragmentChild::SlotElement(el) => precompute_fragment(&mut el.fragment, bump),
        FragmentChild::TitleElement(el) => precompute_fragment(&mut el.fragment, bump),
        FragmentChild::SvelteElement(el) => precompute_fragment(&mut el.fragment, bump),
        FragmentChild::SvelteComponent(c) => precompute_fragment(&mut c.fragment, bump),
        FragmentChild::SvelteBody(el)
        | FragmentChild::SvelteBoundary(el)
        | FragmentChild::SvelteDocument(el)
        | FragmentChild::SvelteFragment(el)
        | FragmentChild::SvelteHead(el)
        | FragmentChild::SvelteWindow(el)
        | FragmentChild::SvelteSelf(el)
        | FragmentChild::SvelteOptions(el) => precompute_fragment(&mut el.fragment, bump),
        FragmentChild::IfBlock(b) => {
            precompute_fragment(&mut b.consequent, bump);
            if let Some(a) = &mut b.alternate {
                precompute_fragment(a, bump);
            }
        }
        FragmentChild::EachBlock(b) => {
            precompute_fragment(&mut b.body, bump);
            if let Some(f) = &mut b.fallback {
                precompute_fragment(f, bump);
            }
        }
        FragmentChild::KeyBlock(b) => precompute_fragment(&mut b.fragment, bump),
        FragmentChild::AwaitBlock(b) => {
            if let Some(p) = &mut b.pending {
                precompute_fragment(p, bump);
            }
            if let Some(t) = &mut b.then {
                precompute_fragment(t, bump);
            }
            if let Some(c) = &mut b.catch_ {
                precompute_fragment(c, bump);
            }
        }
        FragmentChild::SnippetBlock(b) => precompute_fragment(&mut b.body, bump),
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
