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

    /// Which fragment (in `ElementTree.fragments`) owns this element, and
    /// its position within that fragment's child list. Used by the
    /// sibling-combinator pass which mirrors upstream's
    /// `get_possible_element_siblings` (per-fragment traversal).
    pub fragment_id: usize,
    pub index_in_fragment: usize,
    /// The fragment_id of this element's body (the children rendered
    /// inside it). `None` for void elements / RenderTag / SvelteSelf.
    /// Used so sibling-combinator can recurse into Component / SlotElement
    /// bodies (upstream's `get_possible_nested_siblings` for Component).
    pub body_fragment: Option<usize>,
    /// Fragments of `{#snippet}` children declared directly inside a
    /// Component (or SvelteComponent / SvelteSelf). Used by the
    /// sibling-combinator pass to recurse into snippet bodies, matching
    /// upstream's `node.metadata.snippets` walk in
    /// `get_possible_nested_siblings` (css-prune.js:1128).
    pub snippet_fragments: Vec<usize>,
}

/// What kind of block boundary an entry in `ElementTree.blocks` represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    /// `{#if}...{:else}...{/if}` — `branches` carries consequent + (optional) alternate.
    IfBlock,
    /// `{#each}...{:else}...{/each}` — `branches` carries body + (optional) fallback.
    EachBlock,
    /// `{#await}...{:then}...{:catch}...{/await}` — three optional fragments.
    AwaitBlock,
    /// `{#key}...{/key}` — single branch.
    KeyBlock,
    /// `{#snippet}` — single branch. Listed here for completeness; snippet
    /// bodies usually sit at the root of the parent fragment and are
    /// walked via `RenderTag` references.
    SnippetBlock,
}

/// A control-flow boundary inserted into a fragment's child list. Used by
/// `get_possible_element_siblings` to know that crossing this entry might
/// not pass through (depends on whether the block is exhaustive).
#[derive(Debug, Clone)]
pub struct BlockEntry {
    pub kind: BlockKind,
    /// Each branch is a fragment id in `ElementTree.fragments`.
    /// Order: IfBlock = [consequent, (alternate)]; EachBlock = [body, (fallback)];
    /// AwaitBlock = [pending?, then?, catch?]; KeyBlock = [fragment];
    /// SnippetBlock = [body].
    pub branches: Vec<usize>,
    /// `IfBlock` with alternate, `EachBlock` always (it iterates),
    /// `AwaitBlock` with all branches present, `KeyBlock` always —
    /// exhaustive means one branch always renders.
    pub exhaustive: bool,
    /// For `SnippetBlock` only — element indices where this snippet's
    /// content effectively renders. Inline snippets declared inside a
    /// Component get the Component listed here (per upstream's
    /// `snippet.metadata.sites.add(node)` in 2-analyze/index.js:842-849).
    pub sites: Vec<usize>,
    /// For `SnippetBlock` — the snippet's declared name. Used to wire
    /// `{@render name()}` tags to the corresponding snippet block.
    pub snippet_name: Option<String>,
}

/// One entry in a fragment's child list — either a real element node
/// (referenced into `ElementTree.elements`) or a control-flow block
/// boundary (referenced into `ElementTree.blocks`).
#[derive(Debug, Clone, Copy)]
pub enum FragChild {
    Element(usize),
    Block(usize),
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

/// Who owns a fragment in the template.
#[derive(Debug, Clone, Copy)]
pub enum FragmentOwner {
    /// The root fragment (`tree.fragments[0]`).
    Root,
    /// An element's body fragment (e.g. `<div>...</div>`).
    Element(usize),
    /// A control-flow block's branch — `branch_index` is the position
    /// within the block's `branches` list (`0 = consequent / body /
    /// pending`, etc.).
    Block { block_idx: usize, branch_index: usize },
}

/// Flat indexed tree of every element in the template.
#[derive(Debug, Default)]
pub struct ElementTree {
    pub elements: Vec<ElementInfo>,
    /// All control-flow blocks discovered in the template. Indexed from
    /// `FragChild::Block(idx)` entries in `fragments`.
    pub blocks: Vec<BlockEntry>,
    /// All fragments — each one is the children list of a template-level
    /// fragment, block branch, or component slot. Indexed by `fragment_id`
    /// on each `ElementInfo` and by `BlockEntry::branches`. The root
    /// fragment is always at index 0 (added by `collect`).
    pub fragments: Vec<Vec<FragChild>>,
    /// Parallel to `fragments` — what owns each fragment.
    pub fragment_owners: Vec<FragmentOwner>,
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
    // Reserve fragment 0 for the root.
    tree.fragments.push(Vec::new());
    tree.fragment_owners.push(FragmentOwner::Root);
    walk_fragment(fragment, None, Existence::Definite, 0, &mut tree);
    // Wire RenderTags to their snippet block sites — mirrors upstream
    // 2-analyze/visitors/RenderTag.js:37 where `snippet.metadata.sites`
    // gets the render tag added when the snippet binding resolves.
    //
    // Resolution is scope-aware: walk up from the RenderTag's enclosing
    // fragment and pick the closest fragment whose children include a
    // matching `{#snippet}` declaration. This handles shadowing where an
    // inner `{#snippet foo}` shadows an outer one of the same name.
    let render_tag_idxs: Vec<usize> = tree
        .elements
        .iter()
        .enumerate()
        .filter(|(_, e)| e.kind == Some(NodeKind::RenderTag))
        .map(|(i, _)| i)
        .collect();
    for el_idx in render_tag_idxs {
        let Some(name) = tree.elements[el_idx].tag.clone() else { continue };
        if let Some(block_idx) = resolve_snippet_in_scope(&tree, el_idx, &name) {
            tree.blocks[block_idx].sites.push(el_idx);
        }
    }
    tree
}

/// Walk upward from `render_tag_idx`'s fragment, looking for the closest
/// enclosing `{#snippet <name>}` block. Mirrors upstream's binding scope
/// resolution for snippets.
fn resolve_snippet_in_scope(
    tree: &ElementTree,
    render_tag_idx: usize,
    name: &str,
) -> Option<usize> {
    let mut frag = tree.elements[render_tag_idx].fragment_id;
    loop {
        for ch in &tree.fragments[frag] {
            if let FragChild::Block(b) = ch {
                if tree.blocks[*b].kind == BlockKind::SnippetBlock
                    && tree.blocks[*b].snippet_name.as_deref() == Some(name)
                {
                    return Some(*b);
                }
            }
        }
        // Inline component-passed snippets are declared as direct
        // children of the Component's body fragment (not pushed as a
        // FragChild::Block, since SnippetBlock processing only registers
        // a sites attachment). The Component's `snippet_fragments` list
        // captures these — look there too.
        if let FragmentOwner::Element(el) = tree.fragment_owners[frag] {
            for &snip_body in &tree.elements[el].snippet_fragments {
                let owner = tree.fragment_owners[snip_body];
                if let FragmentOwner::Block { block_idx, .. } = owner {
                    if tree.blocks[block_idx].snippet_name.as_deref() == Some(name) {
                        return Some(block_idx);
                    }
                }
            }
        }
        match tree.fragment_owners[frag] {
            FragmentOwner::Root => return None,
            FragmentOwner::Element(el) => {
                frag = tree.elements[el].fragment_id;
            }
            FragmentOwner::Block { block_idx, .. } => match find_block_parent(tree, block_idx) {
                Some(up) => frag = up,
                None => return None,
            },
        }
    }
}

fn find_block_parent(tree: &ElementTree, block_idx: usize) -> Option<usize> {
    for (fid, children) in tree.fragments.iter().enumerate() {
        for ch in children {
            if let FragChild::Block(b) = ch {
                if *b == block_idx {
                    return Some(fid);
                }
            }
        }
    }
    None
}

/// Extract the callee name of a `{@render foo()}` (or `foo`) tag if it's
/// a simple identifier reference, so the wiring pass can map it back to
/// the corresponding `{#snippet foo() ...}` block.
fn render_tag_callee_name(expr: &svelte_js_ast::Expression) -> Option<String> {
    use svelte_js_ast::Expression;
    match expr {
        Expression::Identifier(id) => Some(id.name.clone()),
        Expression::Call(call) => match &call.callee {
            Expression::Identifier(id) => Some(id.name.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// Walk `fragment` and populate `tree.fragments[fragment_id]` with one
/// `FragChild` per top-level child. Each element's
/// `fragment_id`/`index_in_fragment` is set as it's pushed. Returns the
/// list of element indices in this fragment so the caller can wire the
/// flat sibling chain.
fn walk_fragment(
    fragment: &Fragment,
    parent: Option<usize>,
    existence: Existence,
    fragment_id: usize,
    tree: &mut ElementTree,
) -> Vec<usize> {
    let mut element_siblings: Vec<usize> = Vec::new();
    for node in &fragment.nodes {
        walk_child(node, parent, existence, fragment_id, tree, &mut element_siblings);
    }
    link_siblings(&element_siblings, tree);
    element_siblings
}

/// Allocate a new fragment id and walk into it. Returns the new fragment id.
fn make_fragment(
    inner: &Fragment,
    parent: Option<usize>,
    existence: Existence,
    owner: FragmentOwner,
    tree: &mut ElementTree,
) -> usize {
    let id = tree.fragments.len();
    tree.fragments.push(Vec::new());
    tree.fragment_owners.push(owner);
    walk_fragment(inner, parent, existence, id, tree);
    id
}

/// `FragChild::Element(idx)` push helper. Sets the element's fragment
/// position and appends it to the fragment's children list.
fn push_frag_element(tree: &mut ElementTree, fragment_id: usize, el_idx: usize) {
    let pos = tree.fragments[fragment_id].len();
    tree.fragments[fragment_id].push(FragChild::Element(el_idx));
    tree.elements[el_idx].fragment_id = fragment_id;
    tree.elements[el_idx].index_in_fragment = pos;
}

fn push_frag_block(tree: &mut ElementTree, fragment_id: usize, block: BlockEntry) -> usize {
    let block_idx = tree.blocks.len();
    tree.blocks.push(block);
    tree.fragments[fragment_id].push(FragChild::Block(block_idx));
    block_idx
}

/// Reserve a block slot up-front so its branches can use the right
/// `FragmentOwner::Block { block_idx, branch_index }` while being walked.
fn reserve_block(tree: &mut ElementTree, kind: BlockKind) -> usize {
    let idx = tree.blocks.len();
    tree.blocks.push(BlockEntry {
        kind,
        branches: Vec::new(),
        exhaustive: false,
        sites: Vec::new(),
        snippet_name: None,
    });
    idx
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
    fragment_id: usize,
    tree: &mut ElementTree,
    out: &mut Vec<usize>,
) {
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
            push_frag_element(tree, fragment_id, idx);
            out.push(idx);
            let body = make_fragment(&el.fragment, Some(idx), existence, FragmentOwner::Element(idx), tree);
            tree.elements[idx].body_fragment = Some(body);
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
            push_frag_element(tree, fragment_id, idx);
            out.push(idx);
            let body = make_fragment(&c.fragment, Some(idx), own_existence, FragmentOwner::Element(idx), tree);
            tree.elements[idx].body_fragment = Some(body);
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
            push_frag_element(tree, fragment_id, idx);
            out.push(idx);
            let body = make_fragment(&el.fragment, Some(idx), existence, FragmentOwner::Element(idx), tree);
            tree.elements[idx].body_fragment = Some(body);
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
            push_frag_element(tree, fragment_id, idx);
            out.push(idx);
            let body = make_fragment(&el.fragment, Some(idx), own_existence, FragmentOwner::Element(idx), tree);
            tree.elements[idx].body_fragment = Some(body);
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
            push_frag_element(tree, fragment_id, idx);
            out.push(idx);
            let body = make_fragment(&el.fragment, Some(idx), existence, FragmentOwner::Element(idx), tree);
            tree.elements[idx].body_fragment = Some(body);
        }
        FragmentChild::SvelteHead(el) => {
            // <svelte:head> body renders into the document <head> but the
            // wrapper itself isn't a DOM node we can match selectors
            // against. Mirror upstream by treating the children as
            // siblings of the wrapper's parent at the same existence.
            for n in &el.fragment.nodes {
                walk_child(n, parent, existence, fragment_id, tree, out);
            }
        }
        FragmentChild::SvelteFragment(el) => {
            for n in &el.fragment.nodes {
                walk_child(n, parent, existence, fragment_id, tree, out);
            }
        }
        FragmentChild::SvelteSelf(el) => {
            let idx = push_element(
                NodeKind::SvelteSelf,
                None,
                &el.attributes,
                parent,
                existence,
                tree,
            );
            push_frag_element(tree, fragment_id, idx);
            out.push(idx);
        }
        FragmentChild::SvelteBoundary(el) => {
            for n in &el.fragment.nodes {
                walk_child(n, parent, existence, fragment_id, tree, out);
            }
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
            push_frag_element(tree, fragment_id, idx);
            out.push(idx);
            let body = make_fragment(&el.fragment, Some(idx), own_existence, FragmentOwner::Element(idx), tree);
            tree.elements[idx].body_fragment = Some(body);
        }
        FragmentChild::SvelteWindow(_)
        | FragmentChild::SvelteDocument(_)
        | FragmentChild::SvelteOptions(_) => {}
        FragmentChild::IfBlock(b) => {
            let inner = Existence::min(existence, Existence::Probable);
            let block_idx = reserve_block(tree, BlockKind::IfBlock);
            let cons = make_fragment(
                &b.consequent,
                parent,
                inner,
                FragmentOwner::Block { block_idx, branch_index: 0 },
                tree,
            );
            let mut branches = vec![cons];
            let has_alt = b.alternate.is_some();
            if let Some(alt) = &b.alternate {
                let alt_id = make_fragment(
                    alt,
                    parent,
                    inner,
                    FragmentOwner::Block { block_idx, branch_index: 1 },
                    tree,
                );
                branches.push(alt_id);
            }
            for &b_id in &branches {
                for ch in tree.fragments[b_id].clone() {
                    if let FragChild::Element(e) = ch {
                        out.push(e);
                    }
                }
            }
            tree.blocks[block_idx].branches = branches;
            tree.blocks[block_idx].exhaustive = has_alt;
            tree.fragments[fragment_id].push(FragChild::Block(block_idx));
        }
        FragmentChild::EachBlock(b) => {
            let inner = Existence::min(existence, Existence::Probable);
            let block_idx = reserve_block(tree, BlockKind::EachBlock);
            let body = make_fragment(
                &b.body,
                parent,
                inner,
                FragmentOwner::Block { block_idx, branch_index: 0 },
                tree,
            );
            let mut branches = vec![body];
            let has_fallback = b.fallback.is_some();
            if let Some(fb) = &b.fallback {
                branches.push(make_fragment(
                    fb,
                    parent,
                    inner,
                    FragmentOwner::Block { block_idx, branch_index: 1 },
                    tree,
                ));
            }
            for &b_id in &branches {
                for ch in tree.fragments[b_id].clone() {
                    if let FragChild::Element(e) = ch {
                        out.push(e);
                    }
                }
            }
            tree.blocks[block_idx].branches = branches;
            tree.blocks[block_idx].exhaustive = has_fallback;
            tree.fragments[fragment_id].push(FragChild::Block(block_idx));
        }
        FragmentChild::AwaitBlock(b) => {
            let inner = Existence::min(existence, Existence::Probable);
            let block_idx = reserve_block(tree, BlockKind::AwaitBlock);
            let mut branches = Vec::new();
            if let Some(f) = &b.pending {
                let bi = branches.len();
                branches.push(make_fragment(
                    f,
                    parent,
                    inner,
                    FragmentOwner::Block { block_idx, branch_index: bi },
                    tree,
                ));
            }
            if let Some(f) = &b.then {
                let bi = branches.len();
                branches.push(make_fragment(
                    f,
                    parent,
                    inner,
                    FragmentOwner::Block { block_idx, branch_index: bi },
                    tree,
                ));
            }
            if let Some(f) = &b.catch_ {
                let bi = branches.len();
                branches.push(make_fragment(
                    f,
                    parent,
                    inner,
                    FragmentOwner::Block { block_idx, branch_index: bi },
                    tree,
                ));
            }
            for &b_id in &branches {
                for ch in tree.fragments[b_id].clone() {
                    if let FragChild::Element(e) = ch {
                        out.push(e);
                    }
                }
            }
            let exhaustive = b.pending.is_some() && b.then.is_some() && b.catch_.is_some();
            tree.blocks[block_idx].branches = branches;
            tree.blocks[block_idx].exhaustive = exhaustive;
            tree.fragments[fragment_id].push(FragChild::Block(block_idx));
        }
        FragmentChild::KeyBlock(b) => {
            let block_idx = reserve_block(tree, BlockKind::KeyBlock);
            let inner = make_fragment(
                &b.fragment,
                parent,
                existence,
                FragmentOwner::Block { block_idx, branch_index: 0 },
                tree,
            );
            for ch in tree.fragments[inner].clone() {
                if let FragChild::Element(e) = ch {
                    out.push(e);
                }
            }
            tree.blocks[block_idx].branches = vec![inner];
            tree.blocks[block_idx].exhaustive = true;
            tree.fragments[fragment_id].push(FragChild::Block(block_idx));
        }
        FragmentChild::SnippetBlock(s) => {
            // Bodies of `{#snippet name(args)}` children are tracked
            // separately so the sibling-combinator pass can consider them
            // as possible last-elements of the enclosing component.
            //
            // The block IS pushed into the parent fragment as
            // `FragChild::Block` so scope-aware snippet resolution can
            // walk up the fragment list — but it contributes no elements
            // to the parent's sibling chain (snippets only render
            // through `{@render}`).
            let inner = Existence::min(existence, Existence::Probable);
            let snippet_block_idx = reserve_block(tree, BlockKind::SnippetBlock);
            tree.blocks[snippet_block_idx].snippet_name = Some(s.expression.name.clone());
            let body = make_fragment(
                &s.body,
                parent,
                inner,
                FragmentOwner::Block { block_idx: snippet_block_idx, branch_index: 0 },
                tree,
            );
            tree.blocks[snippet_block_idx].branches = vec![body];
            tree.blocks[snippet_block_idx].exhaustive = false;
            tree.fragments[fragment_id].push(FragChild::Block(snippet_block_idx));
            // Attach to the owner element of the enclosing fragment
            // (typically a Component / SvelteComponent / SvelteSelf —
            // root-level snippets aren't attached anywhere here).
            // Mirrors upstream's 2-analyze/index.js:847-849:
            //   `for (const snippet of node.metadata.snippets)
            //      snippet.metadata.sites.add(node)`
            // — making the component a "render site" so the sibling
            // walk past the SnippetBlock can substitute the component's
            // siblings.
            if let FragmentOwner::Element(owner_el) = tree.fragment_owners[fragment_id] {
                let kind = tree.elements[owner_el].kind;
                if matches!(
                    kind,
                    Some(NodeKind::Component)
                        | Some(NodeKind::SvelteComponent)
                        | Some(NodeKind::SvelteSelf)
                ) {
                    tree.elements[owner_el].snippet_fragments.push(body);
                    tree.blocks[snippet_block_idx].sites.push(owner_el);
                }
            }
        }
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
                tag: render_tag_callee_name(&rt.expression),
                parent,
                existence: Existence::min(existence, Existence::Probable),
                ..ElementInfo::default()
            });
            push_frag_element(tree, fragment_id, idx);
            out.push(idx);
        }
        _ => {}
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
            // Concatenate each part. Where a part has multiple statically-
            // known values, cross-product into separate variants so callers
            // can still enumerate (e.g. `class='{ a ? "active":"inactive" }'`
            // → ["active", "inactive"]).
            let mut variants: Vec<String> = vec![String::new()];
            for p in parts {
                match p {
                    AttributeValuePart::Text(t) => {
                        for v in &mut variants {
                            v.push_str(&t.data);
                        }
                    }
                    AttributeValuePart::ExpressionTag(tag) => {
                        let mut sub = AttrValueSet::default();
                        collect_expression_values(&tag.expression, &mut sub);
                        if sub.unknown {
                            out.add_unknown();
                            return;
                        }
                        let mut next = Vec::with_capacity(variants.len() * sub.known.len().max(1));
                        if sub.known.is_empty() {
                            // Treat as empty string.
                            for v in &variants {
                                next.push(v.clone());
                            }
                        } else {
                            for v in &variants {
                                for s in &sub.known {
                                    let mut combined = v.clone();
                                    combined.push_str(s);
                                    next.push(combined);
                                }
                            }
                        }
                        variants = next;
                    }
                }
            }
            for v in variants {
                out.add_known(v);
            }
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
        // `class={[a, b, c]}` — clsx semantics: each element contributes
        // class tokens, all concatenated with whitespace. We track the
        // set of possible whole-attribute strings.
        Expression::Array(arr) => {
            let mut variants: Vec<String> = vec![String::new()];
            for el in &arr.elements {
                let sub_expr = match el {
                    svelte_js_ast::ArrayElement::Expression(e) => e,
                    svelte_js_ast::ArrayElement::Elision => continue,
                    svelte_js_ast::ArrayElement::Spread(_) => {
                        out.add_unknown();
                        return;
                    }
                };
                let mut sub = AttrValueSet::default();
                collect_expression_values(sub_expr, &mut sub);
                if sub.unknown {
                    out.add_unknown();
                    return;
                }
                // Empty contribution counts as "absent" — preserve all
                // existing variants AND merge per-known-value variants.
                let mut next: Vec<String> = Vec::new();
                for prefix in &variants {
                    if sub.known.is_empty() {
                        next.push(prefix.clone());
                        continue;
                    }
                    for v in &sub.known {
                        let mut combined = prefix.clone();
                        if !combined.is_empty() && !v.is_empty() {
                            combined.push(' ');
                        }
                        combined.push_str(v);
                        next.push(combined);
                    }
                }
                variants = next;
            }
            for v in variants {
                out.add_known(v);
            }
        }
        // `class={{ a: true, b: cond, 'c d': true }}` — keys whose value
        // is truthy become tokens. Conservatively treat every key as a
        // possible token (either present or absent), so the cross
        // product covers any combination.
        Expression::Object(obj) => {
            // For each property, possible contributions are: key string
            // (if truthy) or "" (if not). For an object of N properties,
            // 2^N variants is impractical — instead, collect ALL keys
            // into the known set and let downstream split on whitespace.
            let mut acc = String::new();
            let mut any_unknown_key = false;
            for p in &obj.properties {
                match p {
                    svelte_js_ast::ObjectMember::Property(prop) => {
                        let key_str = match &prop.key {
                            svelte_js_ast::PropertyKey::Identifier(id) => Some(id.name.clone()),
                            svelte_js_ast::PropertyKey::Literal(lit) => match lit.as_ref() {
                                svelte_js_ast::Literal::String(s) => Some(s.value.clone()),
                                _ => None,
                            },
                            svelte_js_ast::PropertyKey::Expression(e) => {
                                if let svelte_js_ast::Expression::Literal(lit) = e {
                                    if let svelte_js_ast::Literal::String(s) = lit.as_ref() {
                                        Some(s.value.clone())
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        };
                        if let Some(k) = key_str {
                            if !acc.is_empty() {
                                acc.push(' ');
                            }
                            acc.push_str(&k);
                        } else {
                            any_unknown_key = true;
                        }
                    }
                    svelte_js_ast::ObjectMember::Spread(_) => {
                        any_unknown_key = true;
                    }
                }
            }
            if any_unknown_key {
                out.add_unknown();
            }
            out.add_known(acc);
        }
        // `cond && expr` or `cond || expr`: either `expr` or the falsy/truthy
        // base. Treat as `expr` plus an empty contribution.
        Expression::Logical(l) => {
            collect_expression_values(&l.right, out);
            // Also include "" — the case where the short-circuit yields false.
            out.add_known(String::new());
            let _ = &l.left;
        }
        // `a ? b : c` → values come from EITHER branch. Mirrors upstream's
        // `ConditionalExpression` handling in possible-values analysis.
        Expression::Conditional(c) => {
            collect_expression_values(&c.consequent, out);
            collect_expression_values(&c.alternate, out);
        }
        // `\`foo${x}bar\`` — when every interpolated expression is also
        // statically-known, the full template can be enumerated.
        Expression::Template(t) => {
            let mut variants: Vec<String> =
                vec![t.quasis.first().map(|q| q.cooked.clone()).unwrap_or_default()];
            for (i, expr) in t.expressions.iter().enumerate() {
                let mut sub = AttrValueSet::default();
                collect_expression_values(expr, &mut sub);
                let after = t
                    .quasis
                    .get(i + 1)
                    .map(|q| q.cooked.clone())
                    .unwrap_or_default();
                if sub.unknown {
                    out.add_unknown();
                    return;
                }
                let mut next_variants = Vec::new();
                for prefix in &variants {
                    for v in &sub.known {
                        let mut s = prefix.clone();
                        s.push_str(v);
                        s.push_str(&after);
                        next_variants.push(s);
                    }
                }
                variants = next_variants;
            }
            for v in variants {
                out.add_known(v);
            }
        }
        _ => out.add_unknown(),
    }
}
