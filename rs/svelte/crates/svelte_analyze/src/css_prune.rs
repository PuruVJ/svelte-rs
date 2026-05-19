//! CSS prune pass — full upstream-parity port.
//!
//! Ported from `packages/svelte/src/compiler/phases/2-analyze/css/css-prune.js`
//! (1247 LOC). Layout mirrors upstream: `prune` → `get_relative_selectors` →
//! `truncate` → `apply_selector` → `apply_combinator` →
//! `every_is_global` / `is_global` → `relative_selector_might_apply_to_node` →
//! `attribute_matches` → `test_attribute` plus the tree-traversal helpers
//! `get_ancestor_elements`, `get_descendant_elements`,
//! `get_possible_element_siblings`, `get_possible_nested_siblings`,
//! `loop_child`, `is_block`, `has_definite_elements`.

use std::collections::HashMap;

use svelte_ast::css::{
    AttributeSelector, ClassSelector, Combinator, CombinatorKind, ComplexSelector, IdSelector,
    NestingSelectorName, PseudoClassSelector, RelativeSelector, RelativeSelectorKind, Rule, SelectorList,
    SimpleSelector, StyleSheet, StyleSheetChild, TypeSelector, TypeSelectorKind,
};

use crate::css_analyze::CssAnalysis;
use crate::css_possible_values::PossibleValues;

use crate::css_prune_data::{
    case_insensitive_attributes, whitelist_attribute_selector, Existence,
};
use crate::template_elements::{
    AttrValueSet, BlockKind, ElementInfo, ElementTree, FragChild, NodeKind,
};

/// Direction in which `apply_selector` walks the relative-selector chain.
/// Mirrors `FORWARD` / `BACKWARD` constants in css-prune.js:18-19.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    Forward,
    Backward,
}

/// Singleton descendant combinator — used during NestingSelector / `&`
/// injection when the user wrote a bare `:global(...)` parent. Mirrors
/// `descendant_combinator` in css-prune.js:68-73.
fn descendant_combinator() -> Combinator {
    Combinator {
        kind: CombinatorKind::Combinator,
        name: " ".to_string(),
        start: 0,
        end: 0,
    }
}

/// Singleton implicit nesting selector — used by `get_relative_selectors`
/// when a nested rule has no explicit `&`. Mirrors `nesting_selector` in
/// css-prune.js:75-117.
fn nesting_selector() -> RelativeSelector {
    RelativeSelector {
        kind: RelativeSelectorKind::RelativeSelector,
        start: 0,
        end: 0,
        combinator: None,
        selectors: vec![SimpleSelector::NestingSelector(
            svelte_ast::css::NestingSelector {
                kind: svelte_ast::css::NestingSelectorKind::NestingSelector,
                start: 0,
                end: 0,
                name: NestingSelectorName::Ampersand,
            },
        )],
    }
}

/// Universal `*` selector — used by `:has(...)` matching when we need a
/// selector that matches anything.
fn any_selector() -> RelativeSelector {
    RelativeSelector {
        kind: RelativeSelectorKind::RelativeSelector,
        start: 0,
        end: 0,
        combinator: None,
        selectors: vec![SimpleSelector::TypeSelector(TypeSelector {
            kind: TypeSelectorKind::TypeSelector,
            start: 0,
            end: 0,
            name: "*".to_string(),
        })],
    }
}

/// Entry point. Mirrors `prune(stylesheet, elements)` in css-prune.js:130-162.
pub fn prune(stylesheet: &StyleSheet, tree: &ElementTree, css_meta: &mut CssAnalysis) {
    // Build a lookup so `&` resolution can walk multi-level nesting
    // (a nested rule's `&` resolves to the immediate parent, whose `&`
    // resolves to *its* parent, and so on).
    let mut rules_by_key: HashMap<(u32, u32), &Rule> = HashMap::new();
    collect_rules(stylesheet, &mut rules_by_key);
    walk_stylesheet(stylesheet, tree, css_meta, &rules_by_key);
}

fn collect_rules<'a>(
    stylesheet: &'a StyleSheet,
    out: &mut HashMap<(u32, u32), &'a Rule>,
) {
    for child in &stylesheet.children {
        match child {
            StyleSheetChild::Rule(r) => collect_rules_in_rule(r, out),
            StyleSheetChild::Atrule(at) => collect_rules_in_atrule(at, out),
        }
    }
}

fn collect_rules_in_rule<'a>(rule: &'a Rule, out: &mut HashMap<(u32, u32), &'a Rule>) {
    out.insert((rule.start, rule.end), rule);
    for child in &rule.block.children {
        match child {
            svelte_ast::css::BlockChild::Rule(r) => collect_rules_in_rule(r, out),
            svelte_ast::css::BlockChild::Atrule(at) => collect_rules_in_atrule(at, out),
            svelte_ast::css::BlockChild::Declaration(_) => {}
        }
    }
}

fn collect_rules_in_atrule<'a>(
    at: &'a svelte_ast::css::Atrule,
    out: &mut HashMap<(u32, u32), &'a Rule>,
) {
    if let Some(b) = &at.block {
        for child in &b.children {
            match child {
                svelte_ast::css::BlockChild::Rule(r) => collect_rules_in_rule(r, out),
                svelte_ast::css::BlockChild::Atrule(inner) => collect_rules_in_atrule(inner, out),
                svelte_ast::css::BlockChild::Declaration(_) => {}
            }
        }
    }
}

fn walk_stylesheet<'a>(
    stylesheet: &'a StyleSheet,
    tree: &ElementTree,
    css_meta: &mut CssAnalysis,
    rules_by_key: &HashMap<(u32, u32), &'a Rule>,
) {
    for child in &stylesheet.children {
        match child {
            StyleSheetChild::Rule(rule) => walk_rule(rule, None, tree, css_meta, rules_by_key),
            StyleSheetChild::Atrule(at) => walk_atrule(at, tree, css_meta, rules_by_key),
        }
    }
}

/// Resolve the parent of a rule via its `RuleMetadata::parent_rule_key`,
/// then look up the corresponding `&Rule` via `rules_by_key`. Returns
/// `None` for top-level rules.
fn rule_parent<'a>(
    rule: &Rule,
    css_meta: &CssAnalysis,
    rules_by_key: &HashMap<(u32, u32), &'a Rule>,
) -> Option<&'a Rule> {
    let meta = css_meta.rule_metadata.get(&(rule.start, rule.end))?;
    let key = meta.parent_rule_key?;
    rules_by_key.get(&key).copied()
}

fn walk_rule<'a>(
    rule: &'a Rule,
    parent_rule: Option<&'a Rule>,
    tree: &ElementTree,
    css_meta: &mut CssAnalysis,
    rules_by_key: &HashMap<(u32, u32), &'a Rule>,
) {
    let rule_meta = css_meta
        .rule_metadata
        .get(&(rule.start, rule.end))
        .copied()
        .unwrap_or_default();

    if rule_meta.is_global_block {
        // For a non-lone global block (e.g. `div :global { ... }`) the
        // local prefix (`div`) still needs to be pruned/scoped — the
        // `:global` part just signals that descendants in the body are
        // unscoped. Mirrors upstream's behavior at css/index.js:283 where
        // ComplexSelector visitor still runs on global-block rules.
        for complex in &rule.prelude.children {
            prune_complex_selector(complex, rule, parent_rule, tree, css_meta, rules_by_key);
        }
    } else {
        for complex in &rule.prelude.children {
            prune_complex_selector(complex, rule, parent_rule, tree, css_meta, rules_by_key);
        }
    }

    // Recurse into nested rules. They see `rule` as their parent for the
    // `&` / NestingSelector machinery.
    for child in &rule.block.children {
        match child {
            svelte_ast::css::BlockChild::Rule(nested) => {
                walk_rule(nested, Some(rule), tree, css_meta, rules_by_key)
            }
            svelte_ast::css::BlockChild::Atrule(at) => walk_atrule(at, tree, css_meta, rules_by_key),
            svelte_ast::css::BlockChild::Declaration(_) => {}
        }
    }
}

fn walk_atrule<'a>(
    atrule: &'a svelte_ast::css::Atrule,
    tree: &ElementTree,
    css_meta: &mut CssAnalysis,
    rules_by_key: &HashMap<(u32, u32), &'a Rule>,
) {
    if let Some(block) = &atrule.block {
        for child in &block.children {
            match child {
                svelte_ast::css::BlockChild::Rule(r) => walk_rule(r, None, tree, css_meta, rules_by_key),
                svelte_ast::css::BlockChild::Atrule(at) => walk_atrule(at, tree, css_meta, rules_by_key),
                svelte_ast::css::BlockChild::Declaration(_) => {}
            }
        }
    }
}

/// Apply `complex` against every element. Mirrors the `ComplexSelector`
/// visitor in css-prune.js:139-160.
fn prune_complex_selector<'a>(
    complex: &ComplexSelector,
    rule: &'a Rule,
    parent_rule: Option<&'a Rule>,
    tree: &ElementTree,
    css_meta: &mut CssAnalysis,
    rules_by_key: &HashMap<(u32, u32), &'a Rule>,
) {
    let key = (complex.start, complex.end);
    let already_used = css_meta
        .complex_selector_metadata
        .get(&key)
        .is_some_and(|m| m.used);
    if already_used {
        return;
    }

    let selectors = get_relative_selectors(complex, rule, parent_rule, Some(css_meta));
    if selectors.is_empty() {
        return;
    }

    let mut matched = false;
    for el_idx in tree.indices() {
        let kind = tree.elements[el_idx].kind;
        if !is_match_candidate(kind) {
            continue;
        }
        if apply_selector(
            &selectors,
            rule,
            parent_rule,
            el_idx,
            tree,
            css_meta,
            Direction::Backward,
            0,
            selectors.len(),
            rules_by_key,
        ) {
            matched = true;
        }
    }
    if matched {
        css_meta
            .complex_selector_metadata
            .entry(key)
            .or_default()
            .used = true;
    }
}

/// `is_match_candidate` is true for the kinds of nodes `prune` iterates
/// over. RenderTag and pure-pass-through wrappers are excluded — those
/// only matter as siblings to other elements.
fn is_match_candidate(kind: Option<NodeKind>) -> bool {
    matches!(
        kind,
        Some(NodeKind::RegularElement)
            | Some(NodeKind::Component)
            | Some(NodeKind::SvelteComponent)
            | Some(NodeKind::SvelteSelf)
            | Some(NodeKind::SvelteElement)
            | Some(NodeKind::TitleElement)
            | Some(NodeKind::SlotElement)
            | Some(NodeKind::SvelteBody)
    )
}

/// `get_relative_selectors(node)` — discard trailing `:global(...)` and
/// inject an implicit `&` when this rule is nested but the user didn't
/// write `&` themselves. Mirrors css-prune.js:171-203.
fn get_relative_selectors(
    complex: &ComplexSelector,
    rule: &Rule,
    parent_rule: Option<&Rule>,
    css_meta: Option<&CssAnalysis>,
) -> Vec<RelativeSelector> {
    let mut selectors = truncate_with_meta(complex, css_meta);

    if parent_rule.is_some() && !selectors.is_empty() {
        let mut has_explicit_nesting = false;
        for sel in &selectors {
            if has_nesting_selector(sel) {
                has_explicit_nesting = true;
                break;
            }
        }
        if !has_explicit_nesting {
            // Ensure the first selector has a combinator (default to
            // descendant) so the implicit `&` actually combines.
            if selectors[0].combinator.is_none() {
                let mut clone = selectors[0].clone();
                clone.combinator = Some(descendant_combinator());
                selectors[0] = clone;
            }
            selectors.insert(0, nesting_selector());
        }
    }
    let _ = rule; // rule unused here; matches upstream signature
    selectors
}

fn has_nesting_selector(rel: &RelativeSelector) -> bool {
    for s in &rel.selectors {
        if matches!(s, SimpleSelector::NestingSelector(_)) {
            return true;
        }
        if let SimpleSelector::PseudoClassSelector(p) = s {
            if let Some(args) = &p.args {
                for c in &args.children {
                    for r in &c.children {
                        if has_nesting_selector(r) {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

/// Trim trailing global RelativeSelectors. `truncate(node)` in
/// css-prune.js:209-238.
fn truncate(complex: &ComplexSelector) -> Vec<RelativeSelector> {
    truncate_with_meta(complex, None)
}

/// Same as `truncate`, but uses the analyze-pass `is_global`/`is_global_like`
/// metadata when available to decide which trailing rel sels to drop.
/// Mirrors upstream truncate exactly (css-prune.js:209-220).
fn truncate_with_meta(
    complex: &ComplexSelector,
    css_meta: Option<&CssAnalysis>,
) -> Vec<RelativeSelector> {
    let children = &complex.children;
    let mut keep_to = children.len();
    while keep_to > 0 {
        let rel = &children[keep_to - 1];
        let m = css_meta
            .and_then(|cm| {
                cm.relative_selector_metadata
                    .get(&(rel.start, rel.end))
                    .copied()
            })
            .unwrap_or_default();
        let is_bare_global = matches!(
            rel.selectors.first(),
            Some(SimpleSelector::PseudoClassSelector(p)) if p.name == "global" && p.args.is_none()
        );
        if m.is_global || m.is_global_like || is_bare_global {
            keep_to -= 1;
        } else if css_meta.is_none() && is_only_global_pseudo(rel) {
            // Fallback for callers that don't have metadata available.
            keep_to -= 1;
        } else {
            break;
        }
    }
    // Mirrors upstream css-prune.js:221-231 — for any kept selector that
    // contains `:root`, keep only its `:has(...)` simple selectors.
    // `:root` itself never matches a template element but the `:has`
    // check still applies, so this rewrite lets the pruner see the
    // `:has` against the template.
    children[..keep_to]
        .iter()
        .map(|child| {
            let has_root = child.selectors.iter().any(|s| {
                matches!(s, SimpleSelector::PseudoClassSelector(p) if p.name == "root")
            });
            if !has_root {
                return child.clone();
            }
            let mut filtered = child.clone();
            filtered.selectors.retain(|s| {
                matches!(s, SimpleSelector::PseudoClassSelector(p) if p.name == "has")
            });
            filtered
        })
        .collect()
}

/// True if `rel` consists solely of pseudo classes/elements that are
/// global or `:global`. Used by `truncate`. Mirrors the inline check in
/// the upstream `truncate`.
///
/// Note: `:has`, `:is`, `:where`, `:not` are deliberately excluded —
/// their args constrain matching and their globality depends on whether
/// the args are themselves global. Upstream relies on `metadata.is_global`
/// for those cases.
fn is_only_global_pseudo(rel: &RelativeSelector) -> bool {
    rel.selectors.iter().all(|s| match s {
        SimpleSelector::PseudoClassSelector(p) => {
            p.name == "global"
                || p.name == "scope"
                || p.name == "root"
                || p.name == "host"
                || matches!(
                    p.name.as_str(),
                    "hover"
                        | "active"
                        | "focus"
                        | "focus-within"
                        | "focus-visible"
                        | "visited"
                        | "link"
                        | "target"
                        | "first-child"
                        | "last-child"
                        | "only-child"
                        | "checked"
                        | "disabled"
                        | "enabled"
                        | "empty"
                        | "valid"
                        | "invalid"
                        | "required"
                        | "optional"
                        | "read-only"
                        | "read-write"
                        | "placeholder-shown"
                        | "default"
                        | "indeterminate"
                        | "first-of-type"
                        | "last-of-type"
                        | "only-of-type"
                        | "nth-child"
                        | "nth-last-child"
                        | "nth-of-type"
                        | "nth-last-of-type"
                )
        }
        SimpleSelector::PseudoElementSelector(_) => true,
        _ => false,
    }) && rel
        .selectors
        .iter()
        .any(|s| matches!(s, SimpleSelector::PseudoClassSelector(p) if p.name == "global"))
}

/// Backwards-direction selector application. Mirrors `apply_selector` in
/// css-prune.js:243-279.
#[allow(clippy::too_many_arguments)]
fn apply_selector<'a>(
    selectors: &[RelativeSelector],
    rule: &'a Rule,
    parent_rule: Option<&'a Rule>,
    el_idx: usize,
    tree: &ElementTree,
    css_meta: &mut CssAnalysis,
    direction: Direction,
    from: usize,
    to: usize,
    rules_by_key: &HashMap<(u32, u32), &'a Rule>,
) -> bool {
    if from >= to {
        return false;
    }
    let selector_index = match direction {
        Direction::Forward => from,
        Direction::Backward => to - 1,
    };
    let rest_from = match direction {
        Direction::Forward => from + 1,
        Direction::Backward => from,
    };
    let rest_to = match direction {
        Direction::Forward => to,
        Direction::Backward => to - 1,
    };

    let rel = &selectors[selector_index];

    let matched = relative_selector_might_apply_to_node(
        rel, rule, parent_rule, el_idx, tree, css_meta, direction, rules_by_key,
    ) && apply_combinator(
        rel, selectors, rule, parent_rule, el_idx, tree, css_meta, direction, rest_from, rest_to,
        rules_by_key,
    );

    if matched {
        if !is_outer_global(rel) {
            css_meta
                .relative_selector_metadata
                .entry((rel.start, rel.end))
                .or_default()
                .scoped = true;
        }
        // `element.metadata.scoped = true` — feeds the transform phase
        // when deciding which template elements get the scoping hash
        // class. Mirrors css-prune.js:275.
        css_meta.scoped_elements.insert(el_idx);
    }
    matched
}

/// `apply_combinator(...)` — css-prune.js:291-359.
#[allow(clippy::too_many_arguments)]
fn apply_combinator<'a>(
    relative_selector: &RelativeSelector,
    selectors: &[RelativeSelector],
    rule: &'a Rule,
    parent_rule: Option<&'a Rule>,
    el_idx: usize,
    tree: &ElementTree,
    css_meta: &mut CssAnalysis,
    direction: Direction,
    from: usize,
    to: usize,
    rules_by_key: &HashMap<(u32, u32), &'a Rule>,
) -> bool {
    let combinator = match direction {
        Direction::Forward => {
            if from < to {
                selectors[from].combinator.as_ref()
            } else {
                None
            }
        }
        Direction::Backward => relative_selector.combinator.as_ref(),
    };
    let Some(combinator) = combinator else {
        return true;
    };

    match combinator.name.as_str() {
        " " | ">" => {
            let is_adjacent = combinator.name == ">";
            let parents = match direction {
                Direction::Forward => get_descendant_elements(tree, el_idx, is_adjacent),
                Direction::Backward => get_ancestor_elements(tree, el_idx, is_adjacent),
            };
            let mut parent_matched = false;
            // Iterate deterministically by sorting keys, so test results
            // (and any future cache keys) stay stable across runs. The
            // upstream JS uses a Map which preserves insertion order; we
            // approximate by sorting.
            let mut keys: Vec<usize> = parents.keys().copied().collect();
            keys.sort_unstable();
            for p in keys {
                if apply_selector(
                    selectors, rule, parent_rule, p, tree, css_meta, direction, from, to, rules_by_key,
                ) {
                    parent_matched = true;
                }
            }
            // every_is_global fallback fires when:
            // - we're walking backwards, AND
            // - the descendant combinator has no candidates OR we're in
            //   adjacent mode (`>`) and there's no definite candidate.
            // Mirrors css-prune.js:316-321.
            parent_matched
                || (direction == Direction::Backward
                    && (!is_adjacent || !has_definite_elements(&parents))
                    && every_is_global(selectors, from, to, rule, parent_rule, css_meta))
        }
        "+" | "~" => {
            let siblings = get_possible_element_siblings(tree, el_idx, direction, combinator.name == "+");
            let mut sibling_matched = false;
            let mut keys: Vec<usize> = siblings.keys().copied().collect();
            keys.sort_unstable();
            for possible in keys {
                let k = tree.elements[possible].kind;
                if matches!(
                    k,
                    Some(NodeKind::RenderTag) | Some(NodeKind::SlotElement) | Some(NodeKind::Component)
                ) {
                    // `{@render foo()}<p>foo</p>` with `:global(.x) + p` is a match.
                    if to - from == 1 {
                        let m = css_meta
                            .relative_selector_metadata
                            .get(&(selectors[from].start, selectors[from].end))
                            .copied()
                            .unwrap_or_default();
                        if m.is_global {
                            sibling_matched = true;
                        }
                    }
                } else if apply_selector(
                    selectors, rule, parent_rule, possible, tree, css_meta, direction, from, to,
                    rules_by_key,
                ) {
                    sibling_matched = true;
                }
            }
            sibling_matched
                || (direction == Direction::Backward
                    && get_element_parent(tree, el_idx).is_none()
                    && every_is_global(selectors, from, to, rule, parent_rule, css_meta))
        }
        _ => true,
    }
}

/// `every_is_global(...)` — css-prune.js:368-382.
fn every_is_global(
    selectors: &[RelativeSelector],
    from: usize,
    to: usize,
    rule: &Rule,
    parent_rule: Option<&Rule>,
    css_meta: &CssAnalysis,
) -> bool {
    if from >= to {
        return false;
    }
    for sel in selectors.iter().take(to).skip(from) {
        if !is_global_for_prune(sel, rule, parent_rule, css_meta) {
            return false;
        }
    }
    true
}

/// Mirrors the local `is_global(selector, rule)` in css-prune.js:384 —
/// returns true when the relative selector's analyze metadata flags it as
/// `:global` or `:global_like` (which is how `:host`/`:root`/etc. propagate
/// to the prune fallback). Falls back to the syntactic check.
fn is_global_for_prune(
    rel: &RelativeSelector,
    rule: &Rule,
    parent_rule: Option<&Rule>,
    css_meta: &CssAnalysis,
) -> bool {
    let m = css_meta
        .relative_selector_metadata
        .get(&(rel.start, rel.end))
        .copied()
        .unwrap_or_default();
    if m.is_global || m.is_global_like {
        return true;
    }
    // Mirror upstream's is_global walk per simple selector. For a
    // NestingSelector, consult the parent rule's `has_global_selectors`
    // metadata (which is already computed during analyze) — much simpler
    // than recursing through the parent chain.
    let mut explicit_global = false;
    for s in &rel.selectors {
        let mut nested: Option<&SelectorList> = None;
        let mut can_be_global = false;
        match s {
            SimpleSelector::PseudoClassSelector(p) => {
                if (p.name == "is" || p.name == "where") && p.args.is_some() {
                    nested = p.args.as_ref();
                } else {
                    can_be_global = is_unscoped_pseudo_class(p);
                }
            }
            SimpleSelector::NestingSelector(_) => {
                let Some(parent) = parent_rule else {
                    return false;
                };
                // `&` is global iff the parent rule's prelude has at least
                // one all-global complex selector. The analyze pass already
                // set `has_global_selectors` to exactly that condition
                // (per css_analyze.rs:296-303), so consult it directly
                // instead of recursing into the parent prelude (which
                // would need the parent's own parent chain).
                let parent_meta = css_meta
                    .rule_metadata
                    .get(&(parent.start, parent.end))
                    .copied()
                    .unwrap_or_default();
                if parent_meta.has_global_selectors && !parent_meta.has_local_selectors {
                    return true;
                }
                if parent_meta.has_global_selectors {
                    // Parent has a mix — `&` resolves to whichever, so it
                    // could still match a global context. Treat as global
                    // for the every_is_global fallback to fire (mirrors
                    // upstream's `explicitly_global` accumulator).
                    explicit_global = true;
                    continue;
                }
                return false;
            }
            _ => {
                return false;
            }
        }
        let has_global_selectors = nested
            .map(|list| {
                list.children.iter().any(|complex| {
                    complex
                        .children
                        .iter()
                        .all(|r| is_global_for_prune(r, rule, None, css_meta))
                })
            })
            .unwrap_or(false);
        explicit_global |= has_global_selectors;
        if !has_global_selectors && !can_be_global {
            return false;
        }
    }
    explicit_global || rel.selectors.is_empty()
}

/// Mirrors upstream `is_unscoped_pseudo_class` in
/// packages/svelte/src/compiler/phases/2-analyze/css/utils.js:138-155.
/// A pseudo class is "unscoped" iff scoping wouldn't change its meaning —
/// most pseudo classes (`:hover`, `:root`, etc.) and `:has`/`:is`/`:where`/
/// `:not` whose args are all global.
fn is_unscoped_pseudo_class(p: &PseudoClassSelector) -> bool {
    let scoped = matches!(p.name.as_str(), "has" | "is" | "where")
        || (p.name == "not"
            && p.args
                .as_ref()
                .is_some_and(|a| a.children.iter().any(|c| c.children.len() > 1)));
    if !scoped {
        return true;
    }
    // Otherwise unscoped iff `:has(...)`/`:is(...)`/etc. args are entirely global.
    p.args
        .as_ref()
        .is_some_and(|a| a.children.iter().all(|c| c.children.iter().all(|r| is_global(r))))
}

/// `has_definite_elements(result)` — css-prune.js:1162-1177.
/// Returns true if at least one element in the existence map is DEFINITE.
/// Used by `apply_combinator` to decide whether the `every_is_global`
/// fallback should kick in (it only kicks in when no definite candidate
/// exists for the combinator step).
fn has_definite_elements(result: &HashMap<usize, Existence>) -> bool {
    result.values().any(|e| *e == Existence::Definite)
}

/// Local `is_global` for prune — checks if a relative selector is a `:global`
/// (with or without args). Mirrors css-prune.js:384-434.
fn is_global(rel: &RelativeSelector) -> bool {
    rel.selectors
        .first()
        .is_some_and(|s| matches!(s, SimpleSelector::PseudoClassSelector(p) if p.name == "global"))
}

/// `is_outer_global` from utils.js:163-177 — true if rel starts with
/// `:global` and only pseudo-class / pseudo-element selectors follow.
fn is_outer_global(rel: &RelativeSelector) -> bool {
    let Some(first) = rel.selectors.first() else {
        return false;
    };
    let first_is_global = matches!(
        first,
        SimpleSelector::PseudoClassSelector(p) if p.name == "global"
    );
    if !first_is_global {
        return false;
    }
    if let SimpleSelector::PseudoClassSelector(p) = first {
        if p.args.is_none() {
            return true;
        }
    }
    rel.selectors.iter().all(|s| {
        matches!(
            s,
            SimpleSelector::PseudoClassSelector(_) | SimpleSelector::PseudoElementSelector(_)
        )
    })
}

/// `relative_selector_might_apply_to_node(...)` — css-prune.js:436-675.
#[allow(clippy::too_many_arguments)]
fn relative_selector_might_apply_to_node<'a>(
    rel: &RelativeSelector,
    rule: &'a Rule,
    parent_rule: Option<&'a Rule>,
    el_idx: usize,
    tree: &ElementTree,
    css_meta: &mut CssAnalysis,
    direction: Direction,
    rules_by_key: &HashMap<(u32, u32), &'a Rule>,
) -> bool {
    let _ = direction;
    let element = &tree.elements[el_idx];
    let mut include_self: Option<bool> = None;

    for sel in &rel.selectors {
        match sel {
            // `:has(...)` — descendant walk. Mirrors css-prune.js:441-507.
            SimpleSelector::PseudoClassSelector(p) if p.name == "has" && p.args.is_some() => {
                if include_self.is_none() {
                    // For `:has` inside a global context, include the
                    // current element in the descendant walk.
                    include_self = Some(rule_is_global_context(rule, parent_rule, css_meta));
                }
                let include_self = include_self.unwrap_or(false);
                let args = p.args.as_ref().unwrap();
                let mut matched = false;
                for complex_arg in &args.children {
                    let truncated = truncate(complex_arg);
                    if truncated.is_empty() {
                        css_meta
                            .complex_selector_metadata
                            .entry((complex_arg.start, complex_arg.end))
                            .or_default()
                            .used = true;
                        matched = true;
                        continue;
                    }
                    let first = &truncated[0];
                    let rest: Vec<RelativeSelector> = truncated.iter().skip(1).cloned().collect();

                    if include_self {
                        let mut first_no_combinator = first.clone();
                        first_no_combinator.combinator = None;
                        let mut sel_with_self = vec![first_no_combinator];
                        sel_with_self.extend(rest.clone());
                        if apply_selector(
                            &sel_with_self,
                            rule,
                            parent_rule,
                            el_idx,
                            tree,
                            css_meta,
                            Direction::Forward,
                            0,
                            sel_with_self.len(),
                            rules_by_key,
                        ) {
                            css_meta
                                .complex_selector_metadata
                                .entry((complex_arg.start, complex_arg.end))
                                .or_default()
                                .used = true;
                            matched = true;
                        }
                    }
                    let mut sel_excl_self = vec![any_selector()];
                    let mut first_for_excl = first.clone();
                    if first_for_excl.combinator.is_none() {
                        first_for_excl.combinator = Some(descendant_combinator());
                    }
                    sel_excl_self.push(first_for_excl);
                    sel_excl_self.extend(rest);
                    if apply_selector(
                        &sel_excl_self,
                        rule,
                        parent_rule,
                        el_idx,
                        tree,
                        css_meta,
                        Direction::Forward,
                        0,
                        sel_excl_self.len(),
                        rules_by_key,
                    ) {
                        css_meta
                            .complex_selector_metadata
                            .entry((complex_arg.start, complex_arg.end))
                            .or_default()
                            .used = true;
                        matched = true;
                    }
                }
                if !matched {
                    return false;
                }
                continue;
            }

            // Percentage / Nth — always pass.
            SimpleSelector::Percentage(_) | SimpleSelector::Nth(_) => continue,

            SimpleSelector::PseudoClassSelector(p) => {
                let name = unescape_css_name(&p.name);
                if name == "host" || name == "root" {
                    return false;
                }
                if name == "global"
                    && p.args.is_some()
                    && rel.selectors.len() == 1
                {
                    let args = p.args.as_ref().unwrap();
                    let complex_arg = &args.children[0];
                    return apply_selector(
                        &complex_arg.children,
                        rule,
                        parent_rule,
                        el_idx,
                        tree,
                        css_meta,
                        Direction::Backward,
                        0,
                        complex_arg.children.len(),
                        rules_by_key,
                    );
                }
                if name == "global" && p.args.is_none() {
                    return true;
                }
                if name == "not" {
                    if let Some(args) = &p.args {
                        for complex_arg in &args.children {
                            // mark every nested complex_selector as used
                            mark_all_complex_used(complex_arg, css_meta);
                            let relative = truncate(complex_arg);
                            if complex_arg.children.len() > 1 {
                                for s in &relative {
                                    css_meta
                                        .relative_selector_metadata
                                        .entry((s.start, s.end))
                                        .or_default()
                                        .scoped = true;
                                }
                                // (element.metadata.scoped chain — tracked
                                // on element sidecar; we set it via
                                // best-effort by walking ancestors)
                            }
                        }
                    }
                    continue;
                }
                if (name == "is" || name == "where") && p.args.is_some() {
                    let args = p.args.as_ref().unwrap();
                    let mut matched = false;
                    for complex_arg in &args.children {
                        let relative = truncate(complex_arg);
                        let is_global = relative.is_empty();
                        if is_global {
                            css_meta
                                .complex_selector_metadata
                                .entry((complex_arg.start, complex_arg.end))
                                .or_default()
                                .used = true;
                            matched = true;
                        } else if apply_selector(
                            &relative,
                            rule,
                            parent_rule,
                            el_idx,
                            tree,
                            css_meta,
                            Direction::Backward,
                            0,
                            relative.len(),
                            rules_by_key,
                        ) {
                            css_meta
                                .complex_selector_metadata
                                .entry((complex_arg.start, complex_arg.end))
                                .or_default()
                                .used = true;
                            matched = true;
                        } else if complex_arg.children.len() > 1 {
                            // `:is(.x .y)` may match if `.y` is a
                            // descendant of an ancestor matching `.x`.
                            // Conservatively mark used.
                            css_meta
                                .complex_selector_metadata
                                .entry((complex_arg.start, complex_arg.end))
                                .or_default()
                                .used = true;
                            matched = true;
                            for s in &relative {
                                css_meta
                                    .relative_selector_metadata
                                    .entry((s.start, s.end))
                                    .or_default()
                                    .scoped = true;
                            }
                        }
                    }
                    if !matched {
                        return false;
                    }
                }
                // Other pseudo classes (`:hover`, `:nth-child(...)` etc) — always pass.
                continue;
            }
            SimpleSelector::PseudoElementSelector(_) => continue,

            SimpleSelector::AttributeSelector(a) => {
                if !attribute_selector_matches(a, element) {
                    return false;
                }
            }
            SimpleSelector::ClassSelector(c) => {
                let name = unescape_css_name(&c.name);
                if !attribute_matches(element, "class", Some(&name), Some("~="), false) {
                    return false;
                }
            }
            SimpleSelector::IdSelector(i) => {
                let name = unescape_css_name(&i.name);
                if !attribute_matches(element, "id", Some(&name), Some("="), false) {
                    return false;
                }
            }
            SimpleSelector::TypeSelector(t) => {
                let name = unescape_css_name(&t.name);
                if name != "*"
                    && element.kind != Some(NodeKind::SvelteElement)
                    && element
                        .tag
                        .as_deref()
                        .map(|n| !n.eq_ignore_ascii_case(&name))
                        .unwrap_or(true)
                {
                    return false;
                }
            }
            SimpleSelector::NestingSelector(_) => {
                let Some(parent) = parent_rule else {
                    return false;
                };
                let grandparent = rule_parent(parent, css_meta, rules_by_key);
                let mut matched = false;
                for complex_arg in &parent.prelude.children {
                    let parent_selectors = get_relative_selectors(complex_arg, parent, grandparent, Some(css_meta));
                    // If truncate dropped every rel of the parent prelude
                    // (all of them were global / global-like / `:root`),
                    // then `&` is effectively in a global context — every
                    // element matches it. Mirrors upstream's behavior
                    // where `all` is used as a fallback when truncate
                    // returns an empty list.
                    let all_global = complex_arg.children.iter().all(is_global);
                    let parent_meta = css_meta
                        .rule_metadata
                        .get(&(parent.start, parent.end))
                        .copied()
                        .unwrap_or_default();
                    let empty_after_truncate = parent_selectors.is_empty()
                        && (all_global || parent_meta.has_global_selectors);
                    let mut applied = false;
                    if !parent_selectors.is_empty() {
                        applied = apply_selector(
                            &parent_selectors,
                            parent,
                            grandparent,
                            el_idx,
                            tree,
                            css_meta,
                            Direction::Backward,
                            0,
                            parent_selectors.len(),
                            rules_by_key,
                        );
                    }
                    if applied || all_global || empty_after_truncate {
                        css_meta
                            .complex_selector_metadata
                            .entry((complex_arg.start, complex_arg.end))
                            .or_default()
                            .used = true;
                        matched = true;
                    }
                }
                if !matched {
                    return false;
                }
            }
        }
    }
    true
}

/// Check whether the rule (or any of its parent rules) is inside a global
/// context — used by `:has` to decide whether to include the current
/// element in the descendant walk. Mirrors the `include_self` logic at
/// css-prune.js:443-462.
fn rule_is_global_context(
    rule: &Rule,
    parent_rule: Option<&Rule>,
    css_meta: &CssAnalysis,
) -> bool {
    let _ = css_meta;
    // Walk this rule, then parents up the chain. Any `:root` or
    // `:global(...)` makes the chain global.
    let mut chain: Vec<&Rule> = vec![rule];
    if let Some(p) = parent_rule {
        chain.push(p);
    }
    for r in chain {
        for complex in &r.prelude.children {
            for rel in &complex.children {
                if is_global(rel) {
                    return true;
                }
                for s in &rel.selectors {
                    if let SimpleSelector::PseudoClassSelector(p) = s {
                        if p.name == "root" || (p.name == "global" && p.args.is_some()) {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

fn mark_all_complex_used(complex: &ComplexSelector, css_meta: &mut CssAnalysis) {
    css_meta
        .complex_selector_metadata
        .entry((complex.start, complex.end))
        .or_default()
        .used = true;
    for rel in &complex.children {
        for s in &rel.selectors {
            if let SimpleSelector::PseudoClassSelector(p) = s {
                if let Some(args) = &p.args {
                    for c in &args.children {
                        mark_all_complex_used(c, css_meta);
                    }
                }
            }
        }
    }
}

/// `attribute_selector_matches` — coordinates the AttributeSelector
/// case-insensitivity rules plus the `whitelist_attribute_selector`
/// special-case. Mirrors css-prune.js:602-619.
fn attribute_selector_matches(a: &AttributeSelector, element: &ElementInfo) -> bool {
    // Whitelisted? `<details open>` / `<dialog open>` matches `[open]`
    // even when not present in `attr_names`.
    if let Some(tag) = element.tag.as_deref() {
        let tag_lower = tag.to_ascii_lowercase();
        if let Some(list) = whitelist_attribute_selector().get(tag_lower.as_str()) {
            if list.iter().any(|n| n.eq_ignore_ascii_case(&a.name)) {
                return true;
            }
        }
    }
    let case_insensitive = a
        .flags
        .as_deref()
        .is_some_and(|f| f.contains('i'))
        || (!a.flags.as_deref().is_some_and(|f| f.contains('s'))
            && case_insensitive_attributes().contains(a.name.to_ascii_lowercase().as_str()));
    let value = a.value.as_deref().map(unquote);
    attribute_matches(
        element,
        &a.name,
        value.as_deref(),
        a.matcher.as_deref(),
        case_insensitive,
    )
}

/// `attribute_matches(node, name, expected_value, operator, case_insensitive)` —
/// css-prune.js:713-822.
fn attribute_matches(
    element: &ElementInfo,
    name: &str,
    expected_value: Option<&str>,
    operator: Option<&str>,
    case_insensitive: bool,
) -> bool {
    let name_lower = name.to_ascii_lowercase();
    if element.has_spread_attribute {
        return true;
    }
    // BindDirective on the same name → matches.
    if element.bind_directives.contains(&name_lower)
        || element.bind_directives.iter().any(|n| n.eq_ignore_ascii_case(&name_lower))
    {
        return true;
    }
    // StyleDirective on `style` → matches.
    if element.has_style_directive && name_lower == "style" {
        return true;
    }
    // ClassDirective on `class` — matches always except `~=` requires exact name match.
    if name_lower == "class" {
        if operator == Some("~=") {
            if let Some(ev) = expected_value {
                if element.class_directives.contains(ev) {
                    return true;
                }
            }
        } else if !element.class_directives.is_empty() {
            return true;
        }
    }

    let Some(values) = element.attr_values.get(&name_lower) else {
        // Try original-case match.
        let Some(values) = element.attr_values.get(name) else {
            return false;
        };
        return check_attr_values(values, expected_value, operator, case_insensitive, &name_lower);
    };
    check_attr_values(values, expected_value, operator, case_insensitive, &name_lower)
}

fn check_attr_values(
    values: &AttrValueSet,
    expected_value: Option<&str>,
    operator: Option<&str>,
    case_insensitive: bool,
    name_lower: &str,
) -> bool {
    // Empty attribute (bare `disabled`) → matches only when no operator.
    if values.known.contains("") && !values.unknown && values.known.len() == 1 {
        return operator.is_none();
    }
    let Some(expected) = expected_value else {
        return true;
    };
    if values.unknown {
        return true;
    }
    let mut matched = false;
    for v in &values.known {
        if test_attribute(operator.unwrap_or("="), expected, case_insensitive, v) {
            matched = true;
        }
    }
    if !matched && (name_lower == "class" || name_lower == "style") {
        // For `class`/`style`, the class/style directive case-fallthrough is
        // handled in the directive checks above.
    }
    matched
}

/// `test_attribute(operator, expected, case_insensitive, value)` —
/// css-prune.js:683-704.
fn test_attribute(operator: &str, expected: &str, case_insensitive: bool, value: &str) -> bool {
    let cmp = |a: &str, b: &str| {
        if case_insensitive {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    };
    let starts_with = |a: &str, b: &str| {
        if case_insensitive {
            a.len() >= b.len() && a[..b.len()].eq_ignore_ascii_case(b)
        } else {
            a.starts_with(b)
        }
    };
    let ends_with = |a: &str, b: &str| {
        if case_insensitive {
            a.len() >= b.len() && a[a.len() - b.len()..].eq_ignore_ascii_case(b)
        } else {
            a.ends_with(b)
        }
    };
    let contains = |a: &str, b: &str| {
        if case_insensitive {
            a.to_ascii_lowercase().contains(&b.to_ascii_lowercase())
        } else {
            a.contains(b)
        }
    };

    match operator {
        "=" => cmp(value, expected),
        "~=" => value.split_ascii_whitespace().any(|tok| cmp(tok, expected)),
        "|=" => {
            cmp(value, expected) || starts_with(value, &format!("{expected}-"))
        }
        "^=" => starts_with(value, expected),
        "$=" => ends_with(value, expected),
        "*=" => contains(value, expected),
        _ => false,
    }
}

fn unquote(s: &str) -> String {
    let t = s.trim();
    if t.len() >= 2
        && ((t.starts_with('"') && t.ends_with('"')) || (t.starts_with('\'') && t.ends_with('\'')))
    {
        t[1..t.len() - 1].to_string()
    } else {
        t.to_string()
    }
}

/// `get_ancestor_elements(node, adjacent_only)` — css-prune.js:837-905.
/// Walks the element's parent chain. When the chain reaches a node whose
/// fragment is owned by a `SnippetBlock`, the walker continues from each
/// of that snippet's render-tag sites (per upstream's path/SnippetBlock
/// special case).
fn get_ancestor_elements(
    tree: &ElementTree,
    idx: usize,
    adjacent_only: bool,
) -> HashMap<usize, Existence> {
    let mut out = HashMap::new();
    let mut seen = std::collections::HashSet::new();
    walk_ancestors(tree, idx, adjacent_only, &mut out, &mut seen);
    out
}

fn walk_ancestors(
    tree: &ElementTree,
    idx: usize,
    adjacent_only: bool,
    out: &mut HashMap<usize, Existence>,
    seen: &mut std::collections::HashSet<usize>,
) {
    let mut cur = idx;
    loop {
        if let Some(p) = tree.elements[cur].parent {
            if is_match_candidate(tree.elements[p].kind) {
                let existing = out.get(&p).copied().unwrap_or(Existence::Probable);
                out.insert(p, Existence::max(existing, tree.elements[p].existence));
                // Special case: when ascending through an `<option>` whose
                // enclosing `<select>` contains a `<selectedcontent>`,
                // descendants of `<option>` are also rendered into
                // `<selectedcontent>`. Per css-prune.js:861-888, the
                // `<selectedcontent>` element joins the ancestor set.
                if tree.elements[p]
                    .tag
                    .as_deref()
                    .map(|t| t.eq_ignore_ascii_case("option"))
                    .unwrap_or(false)
                {
                    if let Some(select) = find_ancestor_named(tree, p, "select") {
                        if let Some(sc) = find_selectedcontent_descendant(tree, select) {
                            let existing =
                                out.get(&sc).copied().unwrap_or(Existence::Probable);
                            out.insert(
                                sc,
                                Existence::max(existing, tree.elements[sc].existence),
                            );
                        }
                    }
                }
                if adjacent_only {
                    return;
                }
            }
            cur = p;
            continue;
        }
        // No more parents — see if the enclosing fragment is a snippet body.
        // If so, walk from each render-tag site.
        let frag = tree.elements[cur].fragment_id;
        let mut snippet_block: Option<usize> = None;
        let mut probe_frag = frag;
        loop {
            match tree.fragment_owners[probe_frag] {
                crate::template_elements::FragmentOwner::Root => break,
                crate::template_elements::FragmentOwner::Element(_) => break,
                crate::template_elements::FragmentOwner::Block { block_idx, .. } => {
                    if tree.blocks[block_idx].kind == BlockKind::SnippetBlock {
                        snippet_block = Some(block_idx);
                        break;
                    }
                    // Other blocks: ascend through their position in parent fragment.
                    match find_block_position(tree, block_idx) {
                        Some((up, _)) => {
                            probe_frag = up;
                        }
                        None => return,
                    }
                }
            }
        }
        if let Some(block_idx) = snippet_block {
            if seen.contains(&block_idx) {
                return;
            }
            seen.insert(block_idx);
            for site_idx in tree.blocks[block_idx].sites.clone() {
                // Treat the render tag's element parent (and chain) as
                // ancestors. If the site itself is an element (Component
                // render), include it.
                let site_kind = tree.elements[site_idx].kind;
                if matches!(
                    site_kind,
                    Some(NodeKind::RegularElement) | Some(NodeKind::SvelteElement)
                ) {
                    let existing =
                        out.get(&site_idx).copied().unwrap_or(Existence::Probable);
                    out.insert(
                        site_idx,
                        Existence::max(existing, tree.elements[site_idx].existence),
                    );
                    if adjacent_only {
                        return;
                    }
                }
                walk_ancestors(tree, site_idx, adjacent_only, out, seen);
            }
        }
        return;
    }
}

/// `get_descendant_elements(node, adjacent_only)` — css-prune.js:907-972.
fn get_descendant_elements(
    tree: &ElementTree,
    idx: usize,
    adjacent_only: bool,
) -> HashMap<usize, Existence> {
    let mut out = HashMap::new();
    let mut seen_snippets = std::collections::HashSet::new();
    walk_descendants(tree, idx, adjacent_only, &mut out, &mut seen_snippets);
    // `<selectedcontent>` clones the content of the selected `<option>`,
    // so descendants of `<option>` elements within the enclosing `<select>`
    // also count. Mirrors css-prune.js:941-965.
    let is_selectedcontent = tree.elements[idx]
        .tag
        .as_deref()
        .map(|t| t.eq_ignore_ascii_case("selectedcontent"))
        .unwrap_or(false);
    if is_selectedcontent {
        if let Some(select) = find_ancestor_named(tree, idx, "select") {
            let mut option_descendants: Vec<usize> = Vec::new();
            find_option_descendants(tree, select, &mut option_descendants);
            for opt in option_descendants {
                walk_descendants(tree, opt, adjacent_only, &mut out, &mut seen_snippets);
            }
        }
    }
    out
}

fn find_ancestor_named(tree: &ElementTree, idx: usize, name: &str) -> Option<usize> {
    let mut cur = tree.elements[idx].parent;
    while let Some(p) = cur {
        if tree.elements[p]
            .tag
            .as_deref()
            .map(|t| t.eq_ignore_ascii_case(name))
            .unwrap_or(false)
        {
            return Some(p);
        }
        cur = tree.elements[p].parent;
    }
    None
}

fn find_option_descendants(tree: &ElementTree, idx: usize, out: &mut Vec<usize>) {
    if let Some(body) = tree.elements[idx].body_fragment {
        find_options_in_fragment(tree, body, out);
    }
}

fn find_selectedcontent_descendant(tree: &ElementTree, idx: usize) -> Option<usize> {
    if let Some(body) = tree.elements[idx].body_fragment {
        find_selectedcontent_in_fragment(tree, body)
    } else {
        None
    }
}

fn find_selectedcontent_in_fragment(tree: &ElementTree, fragment_id: usize) -> Option<usize> {
    for ch in tree.fragments[fragment_id].clone() {
        if let FragChild::Element(e) = ch {
            if tree.elements[e]
                .tag
                .as_deref()
                .map(|t| t.eq_ignore_ascii_case("selectedcontent"))
                .unwrap_or(false)
            {
                return Some(e);
            }
            if let Some(body) = tree.elements[e].body_fragment {
                if let Some(sc) = find_selectedcontent_in_fragment(tree, body) {
                    return Some(sc);
                }
            }
        } else if let FragChild::Block(b) = ch {
            for &branch in &tree.blocks[b].branches.clone() {
                if let Some(sc) = find_selectedcontent_in_fragment(tree, branch) {
                    return Some(sc);
                }
            }
        }
    }
    None
}

fn find_options_in_fragment(tree: &ElementTree, fragment_id: usize, out: &mut Vec<usize>) {
    for ch in tree.fragments[fragment_id].clone() {
        if let FragChild::Element(e) = ch {
            if tree.elements[e]
                .tag
                .as_deref()
                .map(|t| t.eq_ignore_ascii_case("option"))
                .unwrap_or(false)
            {
                out.push(e);
            }
            if let Some(body) = tree.elements[e].body_fragment {
                find_options_in_fragment(tree, body, out);
            }
        } else if let FragChild::Block(b) = ch {
            for &branch in &tree.blocks[b].branches.clone() {
                find_options_in_fragment(tree, branch, out);
            }
        }
    }
}

fn walk_descendants(
    tree: &ElementTree,
    idx: usize,
    adjacent_only: bool,
    out: &mut HashMap<usize, Existence>,
    seen_snippets: &mut std::collections::HashSet<usize>,
) {
    // Descend into this node's body fragment (and through RenderTags).
    if let Some(body) = tree.elements[idx].body_fragment {
        walk_fragment_descendants(tree, body, adjacent_only, out, seen_snippets);
    } else if tree.elements[idx].kind == Some(NodeKind::RenderTag) {
        if let Some(name) = &tree.elements[idx].tag {
            if let Some(block_idx) = find_snippet_by_name(tree, name) {
                if !seen_snippets.contains(&block_idx) {
                    seen_snippets.insert(block_idx);
                    for &branch in &tree.blocks[block_idx].branches.clone() {
                        walk_fragment_descendants(
                            tree, branch, adjacent_only, out, seen_snippets,
                        );
                    }
                }
            }
        }
    }
}

fn walk_fragment_descendants(
    tree: &ElementTree,
    fragment_id: usize,
    adjacent_only: bool,
    out: &mut HashMap<usize, Existence>,
    seen_snippets: &mut std::collections::HashSet<usize>,
) {
    for ch in tree.fragments[fragment_id].clone() {
        match ch {
            FragChild::Element(e) => {
                let kind = tree.elements[e].kind;
                if matches!(kind, Some(NodeKind::RegularElement) | Some(NodeKind::SvelteElement)) {
                    let existing = out.get(&e).copied().unwrap_or(Existence::Probable);
                    out.insert(e, Existence::max(existing, tree.elements[e].existence));
                    if !adjacent_only {
                        if let Some(body) = tree.elements[e].body_fragment {
                            walk_fragment_descendants(
                                tree, body, adjacent_only, out, seen_snippets,
                            );
                        }
                    }
                } else if kind == Some(NodeKind::RenderTag) {
                    if let Some(name) = &tree.elements[e].tag {
                        if let Some(block_idx) = find_snippet_by_name(tree, name) {
                            if !seen_snippets.contains(&block_idx) {
                                seen_snippets.insert(block_idx);
                                for &branch in &tree.blocks[block_idx].branches.clone() {
                                    walk_fragment_descendants(
                                        tree, branch, adjacent_only, out, seen_snippets,
                                    );
                                }
                            }
                        }
                    }
                } else {
                    // Pass-through (Component / SvelteComponent / SlotElement /
                    // SvelteSelf / TitleElement / SvelteBody): recurse into
                    // body without adding the wrapper itself.
                    if let Some(body) = tree.elements[e].body_fragment {
                        walk_fragment_descendants(
                            tree, body, adjacent_only, out, seen_snippets,
                        );
                    }
                }
            }
            FragChild::Block(b) => {
                // SnippetBlock declarations don't render inline — skip.
                if tree.blocks[b].kind == BlockKind::SnippetBlock {
                    continue;
                }
                for &branch in &tree.blocks[b].branches.clone() {
                    walk_fragment_descendants(
                        tree, branch, adjacent_only, out, seen_snippets,
                    );
                }
            }
        }
    }
}

/// `get_element_parent(node)` — css-prune.js:974-994. Walks up until we
/// hit a renderable element parent.
fn get_element_parent(tree: &ElementTree, idx: usize) -> Option<usize> {
    let mut cur = tree.elements[idx].parent;
    while let Some(p) = cur {
        if is_match_candidate(tree.elements[p].kind) {
            return Some(p);
        }
        cur = tree.elements[p].parent;
    }
    None
}

/// `get_possible_element_siblings(node, direction, adjacent_only)` —
/// css-prune.js:996-1088.
///
/// Walks the element's parent fragment (and up the fragment ownership
/// chain) collecting elements that could appear as a sibling. Crosses
/// non-exhaustive blocks; stops at exhaustive blocks when adjacent_only.
fn get_possible_element_siblings(
    tree: &ElementTree,
    idx: usize,
    direction: Direction,
    adjacent_only: bool,
) -> HashMap<usize, Existence> {
    let mut seen = std::collections::HashSet::new();
    get_possible_element_siblings_seen(tree, idx, direction, adjacent_only, &mut seen)
}

fn get_possible_element_siblings_seen(
    tree: &ElementTree,
    idx: usize,
    direction: Direction,
    adjacent_only: bool,
    seen: &mut std::collections::HashSet<usize>,
) -> HashMap<usize, Existence> {
    let mut result: HashMap<usize, Existence> = HashMap::new();
    // Position from which to walk: start at element's index, then ascend.
    let mut cur_fragment = tree.elements[idx].fragment_id;
    let mut cur_pos = tree.elements[idx].index_in_fragment;

    loop {
        let done = walk_fragment_siblings(
            tree,
            cur_fragment,
            cur_pos,
            direction,
            adjacent_only,
            &mut result,
        );
        if done {
            return result;
        }

        // Move up: who owns this fragment?
        match tree.fragment_owners[cur_fragment] {
            crate::template_elements::FragmentOwner::Root => break,
            crate::template_elements::FragmentOwner::Element(parent_el) => {
                // Element owns the fragment — Component / SvelteComponent /
                // SvelteSelf / SlotElement are transparent: their bodies
                // may render into the surrounding flow. (Upstream lists
                // SlotElement under `is_block` and walks past it the same
                // way it walks past `Component`.)
                let kind = tree.elements[parent_el].kind;
                let transparent = matches!(
                    kind,
                    Some(NodeKind::Component)
                        | Some(NodeKind::SvelteComponent)
                        | Some(NodeKind::SvelteSelf)
                        | Some(NodeKind::SlotElement)
                );
                if !transparent {
                    break;
                }
                cur_fragment = tree.elements[parent_el].fragment_id;
                cur_pos = tree.elements[parent_el].index_in_fragment;
            }
            crate::template_elements::FragmentOwner::Block { block_idx, branch_index } => {
                // Walking past a block boundary in adjacent mode is
                // permitted (the next iteration continues past it). For
                // EachBlock body specifically, also include the each
                // block's own siblings (wrap-around — iter N+1's prev
                // is iter N's last child).
                let kind = tree.blocks[block_idx].kind;
                if kind == BlockKind::EachBlock && branch_index == 0 {
                    let nested = get_possible_nested_siblings_block(
                        tree,
                        block_idx,
                        direction,
                        adjacent_only,
                    );
                    add_to_map(nested, &mut result);
                }
                if kind == BlockKind::SnippetBlock {
                    if seen.contains(&block_idx) {
                        break;
                    }
                    seen.insert(block_idx);
                    // Snippet body — walk each render site's siblings.
                    // Mirrors css-prune.js:1069-1076: for each site, recurse
                    // into `get_possible_element_siblings(site, ...)`.
                    for site_idx in tree.blocks[block_idx].sites.clone() {
                        let nested = get_possible_element_siblings_seen(
                            tree, site_idx, direction, adjacent_only, seen,
                        );
                        add_to_map(nested, &mut result);
                    }
                    break;
                }
                // Locate the block in its enclosing fragment to continue.
                // Each fragment carries `FragChild::Block(block_idx)` — find it.
                let owner_frag = find_block_position(tree, block_idx);
                match owner_frag {
                    Some((frag, pos)) => {
                        cur_fragment = frag;
                        cur_pos = pos;
                    }
                    None => break,
                }
            }
        }
    }
    result
}

/// Walk one fragment's children from `start_pos` in `direction` (exclusive
/// of `start_pos`), accumulating sibling candidates per upstream's loop
/// body in `get_possible_element_siblings` (css-prune.js:1010-1051).
/// Returns `true` if a definitely-existing sibling stopped the walk — the
/// caller should not ascend (matches upstream's `return result`).
fn walk_fragment_siblings(
    tree: &ElementTree,
    fragment_id: usize,
    start_pos: usize,
    direction: Direction,
    adjacent_only: bool,
    result: &mut HashMap<usize, Existence>,
) -> bool {
    let children = &tree.fragments[fragment_id];
    let mut j = match direction {
        Direction::Forward => start_pos + 1,
        Direction::Backward => start_pos.wrapping_sub(1),
    };
    loop {
        if direction == Direction::Forward && j >= children.len() {
            break;
        }
        if direction == Direction::Backward && j == usize::MAX {
            break;
        }
        match children[j] {
            FragChild::Element(e) => {
                let kind = tree.elements[e].kind;
                match kind {
                    Some(NodeKind::RegularElement) => {
                        // Mirrors upstream css-prune.js:1014-1023 — elements
                        // with `slot=` skip the sibling chain (they go to a
                        // different named slot, not the surrounding flow).
                        let has_slot_attr = tree.elements[e]
                            .attr_names
                            .iter()
                            .any(|n| n.eq_ignore_ascii_case("slot"));
                        if !has_slot_attr {
                            result.insert(
                                e,
                                Existence::max(
                                    *result.get(&e).unwrap_or(&Existence::Probable),
                                    Existence::Definite,
                                ),
                            );
                            if adjacent_only {
                                return true;
                            }
                        }
                    }
                    Some(NodeKind::Component) | Some(NodeKind::SlotElement) => {
                        result.insert(
                            e,
                            Existence::max(
                                *result.get(&e).unwrap_or(&Existence::Probable),
                                Existence::Probable,
                            ),
                        );
                        // Upstream's `get_possible_nested_siblings` recurses
                        // into Component/SlotElement bodies and (for Component)
                        // their snippet bodies.
                        if let Some(body) = tree.elements[e].body_fragment {
                            let nested = loop_child(tree, body, direction, adjacent_only);
                            let demoted: HashMap<usize, Existence> = nested
                                .into_iter()
                                .map(|(k, _)| (k, Existence::Probable))
                                .collect();
                            add_to_map(demoted, result);
                        }
                        for &snip in &tree.elements[e].snippet_fragments {
                            let nested = loop_child(tree, snip, direction, adjacent_only);
                            let demoted: HashMap<usize, Existence> = nested
                                .into_iter()
                                .map(|(k, _)| (k, Existence::Probable))
                                .collect();
                            add_to_map(demoted, result);
                        }
                    }
                    Some(NodeKind::SvelteElement) => {
                        result.insert(
                            e,
                            Existence::max(
                                *result.get(&e).unwrap_or(&Existence::Probable),
                                Existence::Probable,
                            ),
                        );
                    }
                    Some(NodeKind::RenderTag) => {
                        result.insert(
                            e,
                            Existence::max(
                                *result.get(&e).unwrap_or(&Existence::Probable),
                                Existence::Probable,
                            ),
                        );
                        // Recurse into the resolved snippet body — mirrors
                        // upstream css-prune.js:1043-1048 where each
                        // `node.metadata.snippets` is folded in.
                        if let Some(name) = &tree.elements[e].tag {
                            if let Some(block_idx) = find_snippet_by_name(tree, name) {
                                let nested = get_possible_nested_siblings_block(
                                    tree, block_idx, direction, adjacent_only,
                                );
                                add_to_map(nested, result);
                            }
                        }
                    }
                    _ => {}
                }
            }
            FragChild::Block(b) => {
                // SnippetBlock children don't render at their declaration
                // site (they render at `{@render}` call sites elsewhere)
                // — skip them when walking sibling chains.
                if tree.blocks[b].kind == BlockKind::SnippetBlock {
                    j = match direction {
                        Direction::Forward => j + 1,
                        Direction::Backward => j.wrapping_sub(1),
                    };
                    continue;
                }
                let nested = get_possible_nested_siblings_block(tree, b, direction, adjacent_only);
                let nested_has_definite = has_definite_elements(&nested);
                add_to_map(nested, result);
                if adjacent_only && nested_has_definite {
                    return true;
                }
            }
        }
        j = match direction {
            Direction::Forward => j + 1,
            Direction::Backward => j.wrapping_sub(1),
        };
    }
    false
}

/// `get_possible_nested_siblings(block, direction, adjacent_only)` —
/// css-prune.js:1097-1156.
fn get_possible_nested_siblings_block(
    tree: &ElementTree,
    block_idx: usize,
    direction: Direction,
    adjacent_only: bool,
) -> HashMap<usize, Existence> {
    let mut seen = std::collections::HashSet::new();
    get_possible_nested_siblings_block_seen(tree, block_idx, direction, adjacent_only, &mut seen)
}

fn get_possible_nested_siblings_block_seen(
    tree: &ElementTree,
    block_idx: usize,
    direction: Direction,
    adjacent_only: bool,
    seen: &mut std::collections::HashSet<usize>,
) -> HashMap<usize, Existence> {
    // SnippetBlock acts like other blocks but is cycle-prone via RenderTag
    // resolution — `seen` tracks block indices we've already descended into.
    if tree.blocks[block_idx].kind == BlockKind::SnippetBlock {
        if seen.contains(&block_idx) {
            return HashMap::new();
        }
        seen.insert(block_idx);
    }
    let mut result: HashMap<usize, Existence> = HashMap::new();
    let mut exhaustive = tree.blocks[block_idx].exhaustive;
    for &branch_frag in &tree.blocks[block_idx].branches.clone() {
        let map = loop_child_seen(tree, branch_frag, direction, adjacent_only, seen);
        exhaustive &= has_definite_elements(&map);
        add_to_map(map, &mut result);
    }
    if !exhaustive {
        for v in result.values_mut() {
            *v = Existence::Probable;
        }
    }
    result
}

/// `loop_child(children, direction, adjacent_only)` — css-prune.js:1201-1230.
fn loop_child(
    tree: &ElementTree,
    fragment_id: usize,
    direction: Direction,
    adjacent_only: bool,
) -> HashMap<usize, Existence> {
    let mut seen = std::collections::HashSet::new();
    loop_child_seen(tree, fragment_id, direction, adjacent_only, &mut seen)
}

fn loop_child_seen(
    tree: &ElementTree,
    fragment_id: usize,
    direction: Direction,
    adjacent_only: bool,
    seen: &mut std::collections::HashSet<usize>,
) -> HashMap<usize, Existence> {
    let mut result: HashMap<usize, Existence> = HashMap::new();
    let children = &tree.fragments[fragment_id];
    if children.is_empty() {
        return result;
    }
    let mut i: i64 = match direction {
        Direction::Forward => 0,
        Direction::Backward => (children.len() as i64) - 1,
    };
    while i >= 0 && (i as usize) < children.len() {
        match children[i as usize] {
            FragChild::Element(e) => {
                let kind = tree.elements[e].kind;
                match kind {
                    Some(NodeKind::RegularElement) => {
                        result.insert(e, Existence::Definite);
                        if adjacent_only {
                            break;
                        }
                    }
                    Some(NodeKind::SvelteElement) => {
                        result.insert(e, Existence::Probable);
                    }
                    Some(NodeKind::RenderTag) => {
                        result.insert(e, Existence::Probable);
                        if let Some(name) = &tree.elements[e].tag {
                            if let Some(block_idx) = find_snippet_by_name(tree, name) {
                                let nested = get_possible_nested_siblings_block_seen(
                                    tree, block_idx, direction, adjacent_only, seen,
                                );
                                add_to_map(nested, &mut result);
                            }
                        }
                    }
                    Some(NodeKind::SlotElement) => {
                        // Upstream `is_block` includes SlotElement —
                        // recurse into its body for the nested-siblings
                        // map and demote (slot may render nothing →
                        // never exhaustive).
                        if let Some(body) = tree.elements[e].body_fragment {
                            let nested = loop_child_seen(tree, body, direction, adjacent_only, seen);
                            let demoted: HashMap<usize, Existence> = nested
                                .into_iter()
                                .map(|(k, _)| (k, Existence::Probable))
                                .collect();
                            add_to_map(demoted, &mut result);
                        }
                    }
                    _ => {}
                }
            }
            FragChild::Block(b) => {
                // Skip SnippetBlock declarations — they don't render inline.
                if tree.blocks[b].kind == BlockKind::SnippetBlock {
                    i = match direction {
                        Direction::Forward => i + 1,
                        Direction::Backward => i - 1,
                    };
                    continue;
                }
                let child_result =
                    get_possible_nested_siblings_block_seen(tree, b, direction, adjacent_only, seen);
                let had_definite = has_definite_elements(&child_result);
                add_to_map(child_result, &mut result);
                if adjacent_only && had_definite {
                    break;
                }
            }
        }
        i = match direction {
            Direction::Forward => i + 1,
            Direction::Backward => i - 1,
        };
    }
    result
}

fn find_snippet_by_name(tree: &ElementTree, name: &str) -> Option<usize> {
    for (i, b) in tree.blocks.iter().enumerate() {
        if b.kind == BlockKind::SnippetBlock {
            if let Some(n) = &b.snippet_name {
                if n == name {
                    return Some(i);
                }
            }
        }
    }
    None
}

/// Find which (fragment_id, position) holds `FragChild::Block(block_idx)`.
/// Used to ascend past a block during sibling walking.
fn find_block_position(tree: &ElementTree, block_idx: usize) -> Option<(usize, usize)> {
    for (fid, children) in tree.fragments.iter().enumerate() {
        for (pos, ch) in children.iter().enumerate() {
            if let FragChild::Block(b) = ch {
                if *b == block_idx {
                    return Some((fid, pos));
                }
            }
        }
    }
    None
}

fn add_to_map(from: HashMap<usize, Existence>, to: &mut HashMap<usize, Existence>) {
    for (k, v) in from {
        let prev = *to.get(&k).unwrap_or(&Existence::Probable);
        to.insert(k, Existence::max(prev, v));
    }
}

fn is_sibling_candidate(kind: Option<NodeKind>) -> bool {
    matches!(
        kind,
        Some(NodeKind::RegularElement)
            | Some(NodeKind::Component)
            | Some(NodeKind::SvelteComponent)
            | Some(NodeKind::SvelteSelf)
            | Some(NodeKind::SvelteElement)
            | Some(NodeKind::TitleElement)
            | Some(NodeKind::SlotElement)
            | Some(NodeKind::SvelteBody)
            | Some(NodeKind::RenderTag)
    )
}

/// `unescape_css_name` — CSS identifier unescape. Two forms:
/// - `\X` for a non-hex char X → literal X.
/// - `\HH` (up to 6 hex digits) optionally followed by a single
///   whitespace → the unicode codepoint with that scalar value.
/// Mirrors upstream's `unescape` helper in css-prune.js.
fn unescape_css_name(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(name.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '\\' {
            out.push(c);
            i += 1;
            continue;
        }
        // Backslash escape — peek next char(s).
        if i + 1 >= chars.len() {
            // Trailing backslash — treat literally.
            out.push('\\');
            i += 1;
            continue;
        }
        let next = chars[i + 1];
        if !next.is_ascii_hexdigit() {
            out.push(next);
            i += 2;
            continue;
        }
        // Hex escape: up to 6 hex digits, optional trailing whitespace.
        let mut hex = String::new();
        let mut j = i + 1;
        while j < chars.len() && hex.len() < 6 && chars[j].is_ascii_hexdigit() {
            hex.push(chars[j]);
            j += 1;
        }
        if j < chars.len() && chars[j] == ' ' {
            j += 1;
        }
        if let Ok(code) = u32::from_str_radix(&hex, 16) {
            if let Some(c) = char::from_u32(code) {
                out.push(c);
            }
        }
        i = j;
    }
    out
}

// Suppress unused-imports warnings for items reserved for later phases.
#[allow(dead_code)]
fn _suppress_unused(p: &PossibleValues, c: &ClassSelector, i: &IdSelector, ps: &PseudoClassSelector) {
    let _ = (p, c, i, ps);
}
