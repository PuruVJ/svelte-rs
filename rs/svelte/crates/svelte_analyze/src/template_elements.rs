//! Template-element collector.
//!
//! Walks the parsed `Root.fragment` and builds a flat-indexed tree of every
//! node that the CSS prune pass cares about — actual element-like nodes
//! plus the "renderable but not an element" markers (`{@render}` /
//! `<slot>` / `<svelte:component>`) that upstream's `apply_combinator`
//! treats specially. Each entry carries:
//!
//! - tag name (`None` for `<svelte:element this={...}>`, the truly dynamic
//!   case),
//! - the node *kind* so the matcher can distinguish RegularElement /
//!   SvelteElement / Component / SlotElement / RenderTag / etc.,
//! - statically-known class / id / attribute values (with
//!   `AttrValueSet::unknown` for non-literal expressions),
//! - parent / prev-sibling / next-sibling / first-child / last-child
//!   indices for combinator-aware matching,
//! - the existence value (`Definite` if the node renders unconditionally,
//!   `Probable` if it's inside a conditional block like `{#if}` / `{#each}`).

use std::collections::HashSet;

use svelte_ast::{
    AttributeValue, AttributeValuePart, ElementAttribute, Fragment, FragmentChild,
};

use crate::css_prune_data::Existence;

/// What kind of node this entry represents. Matches the relevant
/// `node.type` discriminants from upstream's `apply_combinator` switch
/// (the `'RenderTag' | 'SlotElement' | 'Component'` special-case in
/// css-prune.js:332-339 is why we need to carry the kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    /// `<div>`, `<input>`, etc. — concrete HTML/SVG element.
    RegularElement,
    /// `<MyComponent>` — invocation. Slots may render arbitrary content,
    /// so the matcher treats components as "could be anything" for
    /// adjacent-sibling combinators.
    Component,
    /// `<svelte:component this={...}>`.
    SvelteComponent,
    /// `<svelte:self>`.
    SvelteSelf,
    /// `<svelte:element this={...}>` — dynamic tag.
    SvelteElement,
    /// `<slot>`.
    SlotElement,
    /// `<title>`.
    TitleElement,
    /// `<svelte:body>`.
    SvelteBody,
    /// `<svelte:head>` body (passes through to parent siblings).
    SvelteHead,
    /// `{@render expr}` — tag, not an element. Treated specially by the
    /// adjacent-sibling combinator.
    RenderTag,
}

/// One node from the template. See module docs.
#[derive(Debug, Default, Clone)]
pub struct ElementInfo {
    pub kind: Option<NodeKind>,
    pub tag: Option<String>,
    pub classes: AttrValueSet,
    pub ids: AttrValueSet,
    pub attr_names: HashSet<String>,
    pub attr_values: std::collections::HashMap<String, AttrValueSet>,
    /// What directives this element carries (matters for
    /// `attribute_matches` when querying `class` / `style` against a
    /// `class:foo` / `style:foo` directive). Names only — modifier /
    /// expression details aren't needed for matching.
    pub bind_directives: HashSet<String>,
    pub class_directives: HashSet<String>,
    pub has_style_directive: bool,
    pub has_spread_attribute: bool,
    pub existence: Existence,

    pub parent: Option<usize>,
    pub prev_sibling: Option<usize>,
    pub next_sibling: Option<usize>,
    pub first_child: Option<usize>,
    pub last_child: Option<usize>,
}

impl Default for NodeKind {
    fn default() -> Self {
        NodeKind::RegularElement
    }
}

/// A set of possible string values for an attribute. `unknown` flips when
/// the expression isn't statically analysable (e.g. `class={dynamic}`).
#[derive(Debug, Default, Clone)]
pub struct AttrValueSet {
    pub known: HashSet<String>,
    pub unknown: bool,
}

impl AttrValueSet {
    pub fn add_known(&mut self, value: impl Into<String>) {
        self.known.insert(value.into());
    }
    pub fn add_unknown(&mut self) {
        self.unknown = true;
    }
    pub fn might_contain(&self, value: &str) -> bool {
        self.unknown || self.known.contains(value)
    }
    pub fn is_empty(&self) -> bool {
        !self.unknown && self.known.is_empty()
    }
}

/// Flat indexed tree of every element in the template.
#[derive(Debug, Default)]
pub struct ElementTree {
    pub elements: Vec<ElementInfo>,
}

impl ElementTree {
    pub fn len(&self) -> usize {
        self.elements.len()
    }
    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }
    pub fn get(&self, idx: usize) -> &ElementInfo {
        &self.elements[idx]
    }
    pub fn indices(&self) -> impl Iterator<Item = usize> + '_ {
        0..self.elements.len()
    }
    pub fn ancestors(&self, idx: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut cur = self.elements[idx].parent;
        while let Some(p) = cur {
            out.push(p);
            cur = self.elements[p].parent;
        }
        out
    }
    pub fn descendants(&self, idx: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut stack = self.elements[idx]
            .first_child
            .map(|c| vec![c])
            .unwrap_or_default();
        while let Some(i) = stack.pop() {
            out.push(i);
            let mut sibs = Vec::new();
            let mut sib = self.elements[i].next_sibling;
            while let Some(s) = sib {
                sibs.push(s);
                sib = self.elements[s].next_sibling;
            }
            for s in sibs.into_iter().rev() {
                stack.push(s);
            }
            if let Some(c) = self.elements[i].first_child {
                stack.push(c);
            }
        }
        out
    }
    pub fn prev_siblings(&self, idx: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut cur = self.elements[idx].prev_sibling;
        while let Some(s) = cur {
            out.push(s);
            cur = self.elements[s].prev_sibling;
        }
        out
    }
    pub fn next_siblings(&self, idx: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut cur = self.elements[idx].next_sibling;
        while let Some(s) = cur {
            out.push(s);
            cur = self.elements[s].next_sibling;
        }
        out
    }
}

/// Collect every element-like node from `root.fragment` and build the
/// indexed tree. Elements directly in the root fragment have
/// `Existence::Definite`; elements inside `{#if}` / `{#each}` /
/// `{#await}` / `{#snippet}` etc. inherit `Existence::Probable`.
pub fn collect(fragment: &Fragment) -> ElementTree {
    let mut tree = ElementTree::default();
    walk_fragment(fragment, None, Existence::Definite, &mut tree);
    tree
}

fn walk_fragment(
    fragment: &Fragment,
    parent: Option<usize>,
    existence: Existence,
    tree: &mut ElementTree,
) -> Vec<usize> {
    let mut siblings: Vec<usize> = Vec::new();
    for node in &fragment.nodes {
        let added = walk_child(node, parent, existence, tree);
        siblings.extend(added);
    }
    link_siblings(&siblings, tree);
    siblings
}

/// Walk a fragment that's nested inside a control-flow block (IfBlock,
/// EachBlock, etc.). Returns the contributed elements but DOES NOT call
/// `link_siblings` because the parent's first_child/sibling chain is owned
/// by the outer fragment walker — we just contribute child nodes.
fn walk_fragment_inline(
    fragment: &Fragment,
    parent: Option<usize>,
    existence: Existence,
    tree: &mut ElementTree,
) -> Vec<usize> {
    let mut siblings: Vec<usize> = Vec::new();
    for node in &fragment.nodes {
        let added = walk_child(node, parent, existence, tree);
        siblings.extend(added);
    }
    siblings
}

fn link_siblings(siblings: &[usize], tree: &mut ElementTree) {
    for i in 0..siblings.len() {
        let prev = if i > 0 { Some(siblings[i - 1]) } else { None };
        let next = siblings.get(i + 1).copied();
        let el = &mut tree.elements[siblings[i]];
        if el.prev_sibling.is_none() {
            el.prev_sibling = prev;
        }
        if el.next_sibling.is_none() {
            el.next_sibling = next;
        }
    }
    if let (Some(&first), Some(&last)) = (siblings.first(), siblings.last()) {
        if let Some(parent_idx) = tree.elements[first].parent {
            let parent = &mut tree.elements[parent_idx];
            if parent.first_child.is_none() {
                parent.first_child = Some(first);
            }
            parent.last_child = Some(last);
        }
    }
}

fn walk_child(
    node: &FragmentChild,
    parent: Option<usize>,
    existence: Existence,
    tree: &mut ElementTree,
) -> Vec<usize> {
    match node {
        FragmentChild::RegularElement(el) => {
            let idx = push_element(
                NodeKind::RegularElement,
                Some(el.name.clone()),
                &el.attributes,
                parent,
                existence,
                tree,
            );
            walk_fragment(&el.fragment, Some(idx), existence, tree);
            vec![idx]
        }
        FragmentChild::Component(c) => {
            let own_existence = Existence::min(existence, Existence::Probable);
            let idx = push_element(
                NodeKind::Component,
                Some(c.name.clone()),
                &c.attributes,
                parent,
                own_existence,
                tree,
            );
            walk_fragment(&c.fragment, Some(idx), own_existence, tree);
            vec![idx]
        }
        FragmentChild::TitleElement(el) => {
            let idx = push_element(
                NodeKind::TitleElement,
                Some("title".to_string()),
                &el.attributes,
                parent,
                existence,
                tree,
            );
            walk_fragment(&el.fragment, Some(idx), existence, tree);
            vec![idx]
        }
        FragmentChild::SlotElement(el) => {
            let own_existence = Existence::min(existence, Existence::Probable);
            let idx = push_element(
                NodeKind::SlotElement,
                Some("slot".to_string()),
                &el.attributes,
                parent,
                own_existence,
                tree,
            );
            walk_fragment(&el.fragment, Some(idx), own_existence, tree);
            vec![idx]
        }
        FragmentChild::SvelteBody(el) => {
            let idx = push_element(
                NodeKind::SvelteBody,
                Some("body".to_string()),
                &el.attributes,
                parent,
                existence,
                tree,
            );
            walk_fragment(&el.fragment, Some(idx), existence, tree);
            vec![idx]
        }
        FragmentChild::SvelteHead(el) => {
            // <svelte:head> body renders into the document <head> but the
            // wrapper itself isn't a DOM node we can match selectors
            // against. Mirror upstream by treating the children as
            // siblings of the wrapper's parent at the same existence.
            walk_fragment(&el.fragment, parent, existence, tree)
        }
        FragmentChild::SvelteFragment(el) => walk_fragment(&el.fragment, parent, existence, tree),
        FragmentChild::SvelteSelf(el) => {
            // <svelte:self> is recursive — register an "element-shaped"
            // entry so combinator matching can see it on the sibling
            // chain. The body is intentionally NOT recursed (it'd be
            // infinite).
            let idx = push_element(
                NodeKind::SvelteSelf,
                None,
                &el.attributes,
                parent,
                existence,
                tree,
            );
            vec![idx]
        }
        FragmentChild::SvelteBoundary(el) => {
            walk_fragment(&el.fragment, parent, existence, tree)
        }
        FragmentChild::SvelteElement(el) => {
            let own_existence = Existence::min(existence, Existence::Probable);
            let idx = push_element(
                NodeKind::SvelteElement,
                None,
                &el.attributes,
                parent,
                own_existence,
                tree,
            );
            walk_fragment(&el.fragment, Some(idx), own_existence, tree);
            vec![idx]
        }
        FragmentChild::SvelteWindow(_)
        | FragmentChild::SvelteDocument(_)
        | FragmentChild::SvelteOptions(_) => Vec::new(),
        FragmentChild::IfBlock(b) => {
            let inner = Existence::min(existence, Existence::Probable);
            let mut out = walk_fragment_inline(&b.consequent, parent, inner, tree);
            if let Some(alt) = &b.alternate {
                out.extend(walk_fragment_inline(alt, parent, inner, tree));
            }
            out
        }
        FragmentChild::EachBlock(b) => {
            let inner = Existence::min(existence, Existence::Probable);
            let mut out = walk_fragment_inline(&b.body, parent, inner, tree);
            if let Some(fb) = &b.fallback {
                out.extend(walk_fragment_inline(fb, parent, inner, tree));
            }
            out
        }
        FragmentChild::AwaitBlock(b) => {
            let inner = Existence::min(existence, Existence::Probable);
            let mut out = Vec::new();
            if let Some(f) = &b.pending {
                out.extend(walk_fragment_inline(f, parent, inner, tree));
            }
            if let Some(f) = &b.then {
                out.extend(walk_fragment_inline(f, parent, inner, tree));
            }
            if let Some(f) = &b.catch_ {
                out.extend(walk_fragment_inline(f, parent, inner, tree));
            }
            out
        }
        FragmentChild::KeyBlock(b) => walk_fragment_inline(&b.fragment, parent, existence, tree),
        FragmentChild::SnippetBlock(_) => Vec::new(),
        FragmentChild::RenderTag(rt) => {
            // {@render} renders a snippet's content — its identity is a
            // tag, not an element, but `apply_combinator` treats it as a
            // sibling-position placeholder. Mirror that by emitting an
            // entry with kind=RenderTag and no tag name. Per upstream's
            // `get_possible_element_siblings` (css-prune.js:1043), RenderTag
            // is always treated as `NODE_PROBABLY_EXISTS`.
            let idx = tree.elements.len();
            tree.elements.push(ElementInfo {
                kind: Some(NodeKind::RenderTag),
                tag: None,
                parent,
                existence: Existence::min(existence, Existence::Probable),
                ..ElementInfo::default()
            });
            let _ = rt;
            vec![idx]
        }
        _ => Vec::new(),
    }
}

fn push_element(
    kind: NodeKind,
    tag: Option<String>,
    attrs: &[ElementAttribute],
    parent: Option<usize>,
    existence: Existence,
    tree: &mut ElementTree,
) -> usize {
    let info = describe_element(kind, tag, attrs, parent, existence);
    let idx = tree.elements.len();
    tree.elements.push(info);
    idx
}

fn describe_element(
    kind: NodeKind,
    tag: Option<String>,
    attrs: &[ElementAttribute],
    parent: Option<usize>,
    existence: Existence,
) -> ElementInfo {
    let mut info = ElementInfo {
        kind: Some(kind),
        tag,
        parent,
        existence,
        ..ElementInfo::default()
    };
    for a in attrs {
        match a {
            ElementAttribute::Attribute(attr) => {
                info.attr_names.insert(attr.name.clone());
                let mut values = AttrValueSet::default();
                collect_attribute_values(&attr.value, &mut values);

                if attr.name == "class" {
                    if values.unknown {
                        info.classes.add_unknown();
                    } else {
                        for v in &values.known {
                            for cls in v.split_ascii_whitespace() {
                                if !cls.is_empty() {
                                    info.classes.add_known(cls.to_string());
                                }
                            }
                        }
                    }
                } else if attr.name == "id" {
                    if values.unknown {
                        info.ids.add_unknown();
                    } else {
                        for v in &values.known {
                            info.ids.add_known(v.clone());
                        }
                    }
                }
                info.attr_values.insert(attr.name.clone(), values);
            }
            ElementAttribute::SpreadAttribute(_) => {
                info.has_spread_attribute = true;
            }
            ElementAttribute::BindDirective(b) => {
                info.bind_directives.insert(b.name.clone());
            }
            ElementAttribute::ClassDirective(c) => {
                info.class_directives.insert(c.name.clone());
                info.classes.add_known(c.name.clone());
            }
            ElementAttribute::StyleDirective(_) => {
                info.has_style_directive = true;
            }
            _ => {}
        }
    }
    info
}

fn collect_attribute_values(value: &AttributeValue, out: &mut AttrValueSet) {
    match value {
        AttributeValue::Empty => {
            out.add_known(String::new());
        }
        AttributeValue::Single(tag) => {
            collect_expression_values(&tag.expression, out);
        }
        AttributeValue::Many(parts) => {
            let mut acc = String::new();
            let mut had_unknown = false;
            for p in parts {
                match p {
                    AttributeValuePart::Text(t) => acc.push_str(&t.data),
                    AttributeValuePart::ExpressionTag(tag) => {
                        let mut sub = AttrValueSet::default();
                        collect_expression_values(&tag.expression, &mut sub);
                        if sub.unknown || sub.known.len() != 1 {
                            had_unknown = true;
                        } else if let Some(v) = sub.known.iter().next() {
                            acc.push_str(v);
                        }
                    }
                }
            }
            if had_unknown {
                out.add_unknown();
            }
            out.add_known(acc);
        }
    }
}

fn collect_expression_values(expr: &svelte_js_ast::Expression, out: &mut AttrValueSet) {
    use svelte_js_ast::{Expression, Literal};
    match expr {
        Expression::Literal(lit) => match lit.as_ref() {
            Literal::String(s) => out.add_known(s.value.clone()),
            Literal::Boolean(b) => out.add_known(b.value.to_string()),
            Literal::Number(n) => out.add_known(n.value.to_string()),
            _ => out.add_unknown(),
        },
        _ => out.add_unknown(),
    }
}
