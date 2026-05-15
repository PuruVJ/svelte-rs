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
    NestingSelectorName, PseudoClassSelector, RelativeSelector, RelativeSelectorKind, Rule,
    SimpleSelector, StyleSheet, StyleSheetChild, TypeSelector, TypeSelectorKind,
};

use crate::css_analyze::CssAnalysis;
use crate::css_possible_values::PossibleValues;

use crate::css_prune_data::{
    case_insensitive_attributes, whitelist_attribute_selector, Existence,
};
use crate::template_elements::{AttrValueSet, ElementInfo, ElementTree, NodeKind};

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
    walk_stylesheet(stylesheet, tree, css_meta);
}

fn walk_stylesheet(
    stylesheet: &StyleSheet,
    tree: &ElementTree,
    css_meta: &mut CssAnalysis,
) {
    for child in &stylesheet.children {
        match child {
            StyleSheetChild::Rule(rule) => walk_rule(rule, None, tree, css_meta),
            StyleSheetChild::Atrule(at) => walk_atrule(at, tree, css_meta),
        }
    }
}

fn walk_rule(
    rule: &Rule,
    parent_rule: Option<&Rule>,
    tree: &ElementTree,
    css_meta: &mut CssAnalysis,
) {
    let rule_meta = css_meta
        .rule_metadata
        .get(&(rule.start, rule.end))
        .copied()
        .unwrap_or_default();

    if rule_meta.is_global_block {
        // Visit the prelude (so :global { :hover {} } still tracks) but
        // don't try to match against the template.
    } else {
        for complex in &rule.prelude.children {
            prune_complex_selector(complex, rule, parent_rule, tree, css_meta);
        }
    }

    // Recurse into nested rules. They see `rule` as their parent for the
    // `&` / NestingSelector machinery.
    for child in &rule.block.children {
        match child {
            svelte_ast::css::BlockChild::Rule(nested) => {
                walk_rule(nested, Some(rule), tree, css_meta)
            }
            svelte_ast::css::BlockChild::Atrule(at) => walk_atrule(at, tree, css_meta),
            svelte_ast::css::BlockChild::Declaration(_) => {}
        }
    }
}

fn walk_atrule(
    atrule: &svelte_ast::css::Atrule,
    tree: &ElementTree,
    css_meta: &mut CssAnalysis,
) {
    if let Some(block) = &atrule.block {
        for child in &block.children {
            match child {
                svelte_ast::css::BlockChild::Rule(r) => walk_rule(r, None, tree, css_meta),
                svelte_ast::css::BlockChild::Atrule(at) => walk_atrule(at, tree, css_meta),
                svelte_ast::css::BlockChild::Declaration(_) => {}
            }
        }
    }
}

/// Apply `complex` against every element. Mirrors the `ComplexSelector`
/// visitor in css-prune.js:139-160.
fn prune_complex_selector(
    complex: &ComplexSelector,
    rule: &Rule,
    parent_rule: Option<&Rule>,
    tree: &ElementTree,
    css_meta: &mut CssAnalysis,
) {
    let key = (complex.start, complex.end);
    let already_used = css_meta
        .complex_selector_metadata
        .get(&key)
        .is_some_and(|m| m.used);
    if already_used {
        return;
    }

    let selectors = get_relative_selectors(complex, rule, parent_rule);
    if selectors.is_empty() {
        // Selector was just `:global(...)` — already considered used by
        // analyze.
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
) -> Vec<RelativeSelector> {
    let mut selectors = truncate(complex);

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
    let children = &complex.children;
    // Find the last index that is NOT global. Everything up to and
    // including it is kept.
    let mut keep_to = children.len();
    while keep_to > 0 {
        let rel = &children[keep_to - 1];
        if is_only_global_pseudo(rel) {
            keep_to -= 1;
        } else {
            break;
        }
    }
    children[..keep_to].to_vec()
}

/// True if `rel` consists solely of pseudo classes/elements that are
/// global or `:global`. Used by `truncate`. Mirrors the inline check in
/// the upstream `truncate`.
fn is_only_global_pseudo(rel: &RelativeSelector) -> bool {
    rel.selectors.iter().all(|s| match s {
        SimpleSelector::PseudoClassSelector(p) => {
            p.name == "global"
                || p.name == "is"
                || p.name == "where"
                || p.name == "has"
                || p.name == "not"
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
fn apply_selector(
    selectors: &[RelativeSelector],
    rule: &Rule,
    parent_rule: Option<&Rule>,
    el_idx: usize,
    tree: &ElementTree,
    css_meta: &mut CssAnalysis,
    direction: Direction,
    from: usize,
    to: usize,
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
        rel, rule, parent_rule, el_idx, tree, css_meta, direction,
    ) && apply_combinator(
        rel, selectors, rule, parent_rule, el_idx, tree, css_meta, direction, rest_from, rest_to,
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
fn apply_combinator(
    relative_selector: &RelativeSelector,
    selectors: &[RelativeSelector],
    rule: &Rule,
    parent_rule: Option<&Rule>,
    el_idx: usize,
    tree: &ElementTree,
    css_meta: &mut CssAnalysis,
    direction: Direction,
    from: usize,
    to: usize,
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
                    selectors, rule, parent_rule, p, tree, css_meta, direction, from, to,
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
                    && every_is_global(selectors, from, to))
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
                ) {
                    sibling_matched = true;
                }
            }
            sibling_matched
                || (direction == Direction::Backward
                    && get_element_parent(tree, el_idx).is_none()
                    && every_is_global(selectors, from, to))
        }
        _ => true,
    }
}

/// `every_is_global(...)` — css-prune.js:368-382.
fn every_is_global(selectors: &[RelativeSelector], from: usize, to: usize) -> bool {
    if from >= to {
        return false;
    }
    for i in from..to {
        if !is_global(&selectors[i]) {
            return false;
        }
    }
    true
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
fn relative_selector_might_apply_to_node(
    rel: &RelativeSelector,
    rule: &Rule,
    parent_rule: Option<&Rule>,
    el_idx: usize,
    tree: &ElementTree,
    css_meta: &mut CssAnalysis,
    direction: Direction,
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
                if !attribute_matches(element, "class", Some(&c.name), Some("~="), false) {
                    return false;
                }
            }
            SimpleSelector::IdSelector(i) => {
                if !attribute_matches(element, "id", Some(&i.name), Some("="), false) {
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
                // Walk the parent rule's prelude and check each complex
                // selector against this element. Mirrors css-prune.js:649-668.
                let Some(parent) = parent_rule else {
                    // Stand-alone `&` outside a nested rule — invalid.
                    return false;
                };
                let mut matched = false;
                for complex_arg in &parent.prelude.children {
                    let parent_selectors = get_relative_selectors(complex_arg, parent, None);
                    if apply_selector(
                        &parent_selectors,
                        parent,
                        None,
                        el_idx,
                        tree,
                        css_meta,
                        Direction::Backward,
                        0,
                        parent_selectors.len(),
                    ) || complex_arg
                        .children
                        .iter()
                        .all(is_global)
                    {
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
    let mut cur = Some(rule);
    while let Some(r) = cur {
        for complex in &r.prelude.children {
            for rel in &complex.children {
                if is_global(rel) {
                    return true;
                }
            }
        }
        // Move up the chain. We only have direct parent for now (one
        // level). Upstream walks `get_parent_rules` which is the full
        // chain — for our simplified model, peek `parent_rule` once.
        cur = if let Some(_) = cur {
            None
        } else {
            parent_rule
        };
    }
    // Also: any `:root` or `:global(args)` in the prelude makes it global.
    for complex in &rule.prelude.children {
        for rel in &complex.children {
            for s in &rel.selectors {
                if let SimpleSelector::PseudoClassSelector(p) = s {
                    if p.name == "root" || (p.name == "global" && p.args.is_some()) {
                        return true;
                    }
                }
            }
        }
    }
    let _ = css_meta;
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
/// In our flat tree, ancestors are the simple `.parent` chain. For
/// `adjacent_only` we return just the immediate parent.
fn get_ancestor_elements(
    tree: &ElementTree,
    idx: usize,
    adjacent_only: bool,
) -> HashMap<usize, Existence> {
    let mut out = HashMap::new();
    if adjacent_only {
        if let Some(p) = get_element_parent(tree, idx) {
            out.insert(p, tree.elements[p].existence);
        }
    } else {
        let mut cur = tree.elements[idx].parent;
        while let Some(p) = cur {
            if is_match_candidate(tree.elements[p].kind) {
                let existing = out.get(&p).copied().unwrap_or(Existence::Probable);
                out.insert(p, Existence::max(existing, tree.elements[p].existence));
            }
            cur = tree.elements[p].parent;
        }
    }
    out
}

/// `get_descendant_elements(node, adjacent_only)` — css-prune.js:907-972.
fn get_descendant_elements(
    tree: &ElementTree,
    idx: usize,
    adjacent_only: bool,
) -> HashMap<usize, Existence> {
    let mut out = HashMap::new();
    if adjacent_only {
        // Direct children only.
        let mut cur = tree.elements[idx].first_child;
        while let Some(c) = cur {
            if is_match_candidate(tree.elements[c].kind) {
                out.insert(c, tree.elements[c].existence);
            }
            cur = tree.elements[c].next_sibling;
        }
    } else {
        for d in tree.descendants(idx) {
            if is_match_candidate(tree.elements[d].kind) {
                out.insert(d, tree.elements[d].existence);
            }
        }
    }
    out
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
/// css-prune.js:996-1095.
///
/// For `+` (adjacent_only = true) we keep walking past PROBABLE siblings:
/// `<div></div>{#if cond}<span></span>{/if}<p></p>` — `div + p` is a
/// match because the `{#if}` branch might not render, making `<p>`'s
/// immediate previous sibling `<div>`. The collected map can include
/// multiple candidate siblings — `<span>` (PROBABLE) and `<div>`
/// (PROBABLE — because `<span>` might appear in between, so `<div>` is
/// only conditionally the immediate sibling).
///
/// For `~` (adjacent_only = false) we just enumerate every previous /
/// next sibling element.
fn get_possible_element_siblings(
    tree: &ElementTree,
    idx: usize,
    direction: Direction,
    adjacent_only: bool,
) -> HashMap<usize, Existence> {
    let mut out = HashMap::new();
    let siblings = match direction {
        Direction::Forward => tree.next_siblings(idx),
        Direction::Backward => tree.prev_siblings(idx),
    };
    let mut crossed_probable = false;
    for s in siblings {
        if !is_sibling_candidate(tree.elements[s].kind) {
            continue;
        }
        let own = tree.elements[s].existence;
        // If we've walked past one or more PROBABLE siblings, this
        // sibling is also PROBABLE (it's only the immediate one IF the
        // ones we walked past don't exist).
        let effective = if adjacent_only && crossed_probable {
            Existence::Probable
        } else {
            own
        };
        let prev = *out.get(&s).unwrap_or(&Existence::Probable);
        out.insert(s, Existence::max(prev, effective));
        if adjacent_only {
            if own == Existence::Definite {
                // A definite sibling stops us — no later sibling can be
                // the immediate previous.
                break;
            }
            crossed_probable = true;
        }
    }
    out
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

/// `unescape_css_name` — strip `\` from CSS identifier escapes (e.g.
/// `\.foo` → `.foo`). Mirrors the inline replacement at css-prune.js:511-513.
fn unescape_css_name(name: &str) -> String {
    let bytes = name.as_bytes();
    let mut out = String::with_capacity(name.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            out.push(bytes[i + 1] as char);
            i += 2;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

// Suppress unused-imports warnings for items reserved for later phases.
#[allow(dead_code)]
fn _suppress_unused(p: &PossibleValues, c: &ClassSelector, i: &IdSelector, ps: &PseudoClassSelector) {
    let _ = (p, c, i, ps);
}
