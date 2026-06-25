//! CSS analyze pass.
//!
//! Ported from `packages/svelte/src/compiler/phases/2-analyze/css/css-analyze.js`.
//!
//! Walks the parsed `StyleSheet` and labels each `Rule` / `ComplexSelector` /
//! `RelativeSelector` with metadata used during scoping:
//! - which selectors are `:global` (left unscoped) vs scoped,
//! - which selectors are global-LIKE (e.g. `:root`, `:host`,
//!   `::view-transition`) and so should not be scoped,
//! - which rules are global blocks (`:global { ... }`),
//! - the parent-rule chain (for nested rules).
//!
//! The CSS AST itself is immutable after parse. We attach metadata via
//! sidecar `HashMap`s keyed on `(start, end)` byte spans (uniquely identify
//! each node within a stylesheet). Downstream passes (`css_prune`,
//! transforms) read these maps to decide what to scope.

use std::collections::HashMap;

use svelte_ast::css::{
    ComplexSelector, RelativeSelector, Rule, SimpleSelector, StyleSheet, StyleSheetChild,
};

/// Metadata for the whole stylesheet plus per-node sidecar maps.
#[derive(Debug, Default)]
pub struct CssAnalysis {
    /// `@keyframes name { ... }` declarations. Recorded so the scoping pass
    /// can rename them (so a component's `@keyframes` doesn't collide with
    /// another component's keyframes of the same name).
    pub keyframes: Vec<String>,
    /// True if any `:global` selector or `@keyframes -global-foo { ... }`
    /// makes part of the output unscoped.
    pub has_global: bool,
    pub rule_metadata: HashMap<NodeKey, RuleMetadata>,
    pub complex_selector_metadata: HashMap<NodeKey, ComplexSelectorMetadata>,
    pub relative_selector_metadata: HashMap<NodeKey, RelativeSelectorMetadata>,
    /// Template-element indices (into `ElementTree.elements`) that need
    /// the scoping hash class added by the transform phase. Mirrors
    /// `element.metadata.scoped = true` upstream — set by `apply_selector`
    /// every time a selector matches an element.
    pub scoped_elements: std::collections::HashSet<usize>,
}

/// Stable identifier for a CSS AST node within a single stylesheet.
pub type NodeKey = (u32, u32);

#[derive(Debug, Default, Clone, Copy)]
pub struct RuleMetadata {
    pub has_global_selectors: bool,
    pub has_local_selectors: bool,
    /// True for `:global { ... }` style rules whose body is entirely
    /// unscoped.
    pub is_global_block: bool,
    /// The parent rule's NodeKey if this rule is nested. Used by the
    /// pruner to resolve `&` chains in multi-level nesting.
    pub parent_rule_key: Option<NodeKey>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ComplexSelectorMetadata {
    /// True if every relative selector in this complex selector is either
    /// `is_global` or `is_global_like`.
    pub is_global: bool,
    pub used: bool,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct RelativeSelectorMetadata {
    /// `:global(...)` or bare `:global` (with no other scoping selectors).
    pub is_global: bool,
    /// `:root`, `:host`, `::view-transition*`, or any selector that follows
    /// `:global` in the same complex chain.
    pub is_global_like: bool,
    /// Set by the prune pass when the selector matches an element in the
    /// template. Not set here.
    pub scoped: bool,
}

/// Analyze a stylesheet and produce per-node metadata.
pub fn analyze_css(stylesheet: &StyleSheet) -> CssAnalysis {
    let (a, _err) = analyze_css_inner(stylesheet);
    a
}

/// Like `analyze_css`, but also returns the first compile error encountered
/// (e.g. `css_selector_invalid` for a top-level rule whose first relative
/// selector has a combinator). Mirrors upstream's behavior where this is a
/// hard error during analyze.
pub fn analyze_css_with_errors(
    stylesheet: &StyleSheet,
) -> (CssAnalysis, Option<svelte_diagnostics::CompileDiagnostic>) {
    analyze_css_inner(stylesheet)
}

fn analyze_css_inner(
    stylesheet: &StyleSheet,
) -> (CssAnalysis, Option<svelte_diagnostics::CompileDiagnostic>) {
    let mut a = CssAnalysis::default();
    let mut err: Option<svelte_diagnostics::CompileDiagnostic> = None;
    for child in &stylesheet.children {
        match child {
            StyleSheetChild::Rule(rule) => analyze_rule(rule, &mut a, None, &mut err),
            StyleSheetChild::Atrule(atrule) => analyze_atrule(atrule, &mut a, false, &mut err),
        }
    }
    (a, err)
}

fn analyze_atrule(
    atrule: &svelte_ast::css::Atrule,
    a: &mut CssAnalysis,
    inside_global_block: bool,
    err: &mut Option<svelte_diagnostics::CompileDiagnostic>,
) {
    // `@keyframes`: track for scoping. `-global-` prefix means we shouldn't
    // rename. The name is `atrule.prelude.trim()` for `keyframes`.
    if is_keyframes_name(&atrule.name) {
        let prelude = atrule.prelude.trim();
        if prelude.starts_with("-global-") {
            a.has_global = true;
        } else if !inside_global_block {
            a.keyframes.push(prelude.to_string());
        }
    }
    if let Some(block) = &atrule.block {
        for child in &block.children {
            match child {
                svelte_ast::css::BlockChild::Rule(r) => analyze_rule(r, a, None, err),
                svelte_ast::css::BlockChild::Atrule(at) => {
                    analyze_atrule(at, a, inside_global_block, err)
                }
                svelte_ast::css::BlockChild::Declaration(_) => {}
            }
        }
    }
}

fn analyze_rule(
    rule: &Rule,
    a: &mut CssAnalysis,
    parent_rule: Option<&Rule>,
    err: &mut Option<svelte_diagnostics::CompileDiagnostic>,
) {
    let mut meta = RuleMetadata::default();

    // First pass: detect `:global { ... }` block-rule. Walks complex
    // selectors looking for a `:global` PseudoClassSelector with no args
    // at the head. Mirrors css-analyze.js:201-264. Also propagates
    // `is_global_like = true` to relative selectors that follow a `:global`
    // within the same complex selector (so `:global div` → div is_global_like).
    let nselectors = rule.prelude.children.len();
    for (_prelude_idx, complex) in rule.prelude.children.iter().enumerate() {
        let mut after_global = false;
        let mut this_is_global_block = false;
        for (rel_idx, rel) in complex.children.iter().enumerate() {
            // Find first `:global` selector in this relative selector.
            let g_idx = rel.selectors.iter().position(|s| {
                matches!(
                    s,
                    SimpleSelector::PseudoClassSelector(p) if p.name == "global" && p.args.is_none()
                )
            });
            let starts_global = matches!(g_idx, Some(0));
            if starts_global {
                if err.is_none() {
                    // `:global xyz` with siblings + first relative + non-nested
                    // → css_global_block_invalid_modifier_start.
                    if rel.selectors.len() > 1
                        && rel_idx == 0
                        && parent_rule.is_none()
                    {
                        let next = &rel.selectors[1];
                        let span = simple_span(next);
                        *err = Some(svelte_diagnostics::errors::css_global_block_invalid_modifier_start(
                            Some(span),
                        ));
                    } else {
                        meta.is_global_block = true;
                        this_is_global_block = true;
                        after_global = true;
                        // `>:global` etc. — non-space combinator on a :global
                        // → css_global_block_invalid_combinator.
                        if let Some(combinator) = &rel.combinator {
                            if combinator.name != " " {
                                *err = Some(svelte_diagnostics::errors::css_global_block_invalid_combinator(
                                    Some((combinator.start, combinator.end)),
                                    &combinator.name,
                                ));
                            }
                        }
                        // Lone `:global { color: red }` (no descendants, one
                        // selector in the prelude) → invalid_declaration.
                        let is_lone_global =
                            complex.children.len() == 1 && complex.children[0].selectors.len() == 1;
                        if is_lone_global && nselectors > 1 {
                            *err = Some(svelte_diagnostics::errors::css_global_block_invalid_list(
                                Some((rule.prelude.start, rule.prelude.end)),
                            ));
                        }
                        if err.is_none() {
                            let has_decl = rule.block.children.iter().any(|c| {
                                matches!(c, svelte_ast::css::BlockChild::Declaration(_))
                            });
                            if is_lone_global && nselectors == 1 && has_decl {
                                if let Some(decl) = rule.block.children.iter().find_map(|c| {
                                    if let svelte_ast::css::BlockChild::Declaration(d) = c {
                                        Some(d)
                                    } else {
                                        None
                                    }
                                }) {
                                    *err = Some(svelte_diagnostics::errors::css_global_block_invalid_declaration(
                                        Some((decl.start, decl.end)),
                                    ));
                                }
                            }
                        }
                    }
                }
            } else if let Some(idx) = g_idx {
                // `:global` not at idx 0 — e.g. `.x:global` → invalid_modifier.
                if err.is_none() {
                    let span = simple_span(&rel.selectors[idx]);
                    *err = Some(svelte_diagnostics::errors::css_global_block_invalid_modifier(
                        Some(span),
                    ));
                }
            } else if after_global {
                a.relative_selector_metadata
                    .entry(node_key(rel.start, rel.end))
                    .or_default()
                    .is_global_like = true;
            }
        }
        // If THIS prelude is a `:global` block but another prelude isn't a
        // global-block, the whole list is invalid (`:global, .y {...}`).
        if meta.is_global_block && !this_is_global_block && err.is_none() {
            *err = Some(svelte_diagnostics::errors::css_global_block_invalid_list(
                Some((rule.prelude.start, rule.prelude.end)),
            ));
        }
    }

    // Walk selectors to populate complex / relative metadata.
    for complex in &rule.prelude.children {
        // `:is(:global)` / `:where(:global)` etc. — :global inside a
        // pseudoclass argument is css_global_block_invalid_placement.
        for rel in &complex.children {
            for s in &rel.selectors {
                if let SimpleSelector::PseudoClassSelector(p) = s {
                    if let Some(args) = &p.args {
                        for inner_complex in &args.children {
                            for inner_rel in &inner_complex.children {
                                for inner_s in &inner_rel.selectors {
                                    if let SimpleSelector::PseudoClassSelector(ip) = inner_s {
                                        if ip.name == "global" && ip.args.is_none() {
                                            if err.is_none() {
                                                *err = Some(
                                                    svelte_diagnostics::errors::css_global_block_invalid_placement(
                                                        Some((ip.start, ip.end)),
                                                    ),
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        analyze_complex_selector(complex, a);
        // `css_selector_invalid`: a top-level (non-nested) rule whose first
        // relative selector has a leading combinator is invalid. Mirrors
        // `css-analyze.js:145-152`. Skip if we're inside a parent rule
        // (nesting), since nested rules legitimately start with `&` or a
        // combinator pointing at the parent.
        if parent_rule.is_none() {
            if let Some(first) = complex.children.first() {
                if let Some(combinator) = &first.combinator {
                    if err.is_none() {
                        *err = Some(svelte_diagnostics::errors::css_selector_invalid(Some((
                            combinator.start,
                            combinator.end,
                        ))));
                    }
                }
            }
        }
        // `:global(...)` placement / list validity. Mirrors css-analyze.js:67-116.
        validate_global_placement(complex, err);
    }

    for complex in &rule.prelude.children {
        let key = node_key(complex.start, complex.end);
        if let Some(cm) = a.complex_selector_metadata.get(&key) {
            if cm.is_global {
                meta.has_global_selectors = true;
            } else {
                meta.has_local_selectors = true;
            }
        }
    }

    // A rule contributes to `has_global` if it has a global selector AND
    // declarations (so the output really does include unscoped CSS).
    // Mirrors css-analyze.js:279-282.
    if meta.has_global_selectors && rule.block.children.iter().any(|c| {
        matches!(c, svelte_ast::css::BlockChild::Declaration(_))
    }) {
        a.has_global = true;
    }
    if meta.is_global_block {
        a.has_global = true;
    }

    if let Some(p) = parent_rule {
        meta.parent_rule_key = Some(node_key(p.start, p.end));
    }
    a.rule_metadata.insert(node_key(rule.start, rule.end), meta);

    // Validate every NestingSelector inside this rule's prelude. Mirrors
    // upstream's NestingSelector visitor at css-analyze.js:289-313.
    // At the top level, `&` must be the head selector of the first
    // complex inside a lone `:global(...)` arg list. Otherwise it's
    // `css_nesting_selector_invalid_placement`.
    if parent_rule.is_none() {
        let is_lone_global_args = rule.prelude.children.len() == 1
            && rule.prelude.children[0].children.len() == 1
            && rule.prelude.children[0].children[0].selectors.len() == 1
            && matches!(
                rule.prelude.children[0].children[0].selectors.first(),
                Some(SimpleSelector::PseudoClassSelector(p))
                    if p.name == "global" && p.args.is_some()
            );
        let head_nest_in_global = if is_lone_global_args {
            if let Some(SimpleSelector::PseudoClassSelector(p)) =
                rule.prelude.children[0].children[0].selectors.first()
            {
                p.args
                    .as_ref()
                    .and_then(|sl| sl.children.first())
                    .and_then(|c| c.children.first())
                    .and_then(|rel| rel.selectors.first())
                    .map(|s| {
                        if let SimpleSelector::NestingSelector(n) = s {
                            Some((n.start, n.end))
                        } else {
                            None
                        }
                    })
                    .flatten()
            } else {
                None
            }
        } else {
            None
        };
        let mut nest_positions: Vec<(u32, u32)> = Vec::new();
        for complex in &rule.prelude.children {
            collect_nesting_selectors(complex, &mut nest_positions);
        }
        for span in nest_positions {
            // Skip the legal head-of-:global(...) case.
            if Some(span) == head_nest_in_global {
                continue;
            }
            if err.is_none() {
                *err = Some(
                    svelte_diagnostics::errors::css_nesting_selector_invalid_placement(
                        Some(span),
                    ),
                );
            }
        }
    }

    // `:global { &.x { … } }` at the top level — the nested rule starts
    // with `&`, but its only ancestor is a bare `:global` block (no
    // surrounding selector), so `&` has nothing to bind to. Mirrors
    // upstream's `css_global_block_invalid_modifier_start` check.
    if meta.is_global_block && parent_rule.is_none() {
        let is_lone_global_block = rule.prelude.children.iter().all(|c| {
            c.children.len() == 1
                && c.children[0].selectors.len() == 1
                && matches!(
                    c.children[0].selectors.first(),
                    Some(SimpleSelector::PseudoClassSelector(p))
                        if p.name == "global" && p.args.is_none()
                )
        });
        if is_lone_global_block {
            for child in &rule.block.children {
                if let svelte_ast::css::BlockChild::Rule(nested) = child {
                    if let Some(first_complex) = nested.prelude.children.first() {
                        if let Some(first_rel) = first_complex.children.first() {
                            if matches!(
                                first_rel.selectors.first(),
                                Some(SimpleSelector::NestingSelector(_))
                            ) {
                                if err.is_none() {
                                    let span = simple_span(&first_rel.selectors[0]);
                                    *err = Some(
                                        svelte_diagnostics::errors::css_global_block_invalid_modifier_start(
                                            Some(span),
                                        ),
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Recurse into nested rules.
    for child in &rule.block.children {
        match child {
            svelte_ast::css::BlockChild::Rule(nested) => analyze_rule(nested, a, Some(rule), err),
            svelte_ast::css::BlockChild::Atrule(at) => {
                analyze_atrule(at, a, meta.is_global_block, err)
            }
            svelte_ast::css::BlockChild::Declaration(_) => {}
        }
    }
    let _ = parent_rule;
}

/// Recursively walk a ComplexSelector collecting every NestingSelector's
/// span — including ones nested inside `:has/:is/:where/:not/:global` args.
fn collect_nesting_selectors(complex: &ComplexSelector, out: &mut Vec<(u32, u32)>) {
    for rel in &complex.children {
        for s in &rel.selectors {
            match s {
                SimpleSelector::NestingSelector(n) => {
                    out.push((n.start, n.end));
                }
                SimpleSelector::PseudoClassSelector(p) => {
                    if let Some(args) = &p.args {
                        for arg in &args.children {
                            collect_nesting_selectors(arg, out);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn analyze_complex_selector(complex: &ComplexSelector, a: &mut CssAnalysis) {
    // Analyze each relative selector first so we can roll up "all global"
    // into the complex's `is_global`.
    for rel in &complex.children {
        analyze_relative_selector(rel, a);
    }

    let all_global = !complex.children.is_empty()
        && complex.children.iter().all(|rel| {
            let k = node_key(rel.start, rel.end);
            a.relative_selector_metadata
                .get(&k)
                .is_some_and(|m| m.is_global || m.is_global_like)
        });
    a.complex_selector_metadata.insert(
        node_key(complex.start, complex.end),
        ComplexSelectorMetadata {
            is_global: all_global,
            used: all_global, // global selectors are always considered "used"
        },
    );
}

fn analyze_relative_selector(rel: &RelativeSelector, a: &mut CssAnalysis) {
    let mut meta = RelativeSelectorMetadata::default();

    if !rel.selectors.is_empty() && is_global(rel) {
        meta.is_global = true;
    }

    // `:root` / `:host` / `::view-transition*` — global-LIKE.
    let only_pseudo = rel.selectors.iter().all(|s| {
        matches!(
            s,
            SimpleSelector::PseudoClassSelector(_) | SimpleSelector::PseudoElementSelector(_)
        )
    });
    if !rel.selectors.is_empty() && only_pseudo {
        if let Some(first) = rel.selectors.first() {
            match first {
                SimpleSelector::PseudoClassSelector(p) if p.name == "host" => {
                    meta.is_global_like = true;
                }
                SimpleSelector::PseudoElementSelector(p)
                    if matches!(
                        p.name.as_ref(),
                        "view-transition"
                            | "view-transition-group"
                            | "view-transition-old"
                            | "view-transition-new"
                            | "view-transition-image-pair"
                    ) =>
                {
                    meta.is_global_like = true;
                }
                _ => {}
            }
        }
    }
    // `:root`-having relative selectors are global-like UNLESS they also
    // have a `:has(...)` constraint (`:root.y:has(.x)`).
    if rel.selectors.iter().any(|s| {
        matches!(s, SimpleSelector::PseudoClassSelector(p) if p.name == "root")
    }) && !rel.selectors.iter().any(|s| {
        matches!(s, SimpleSelector::PseudoClassSelector(p) if p.name == "has")
    }) {
        meta.is_global_like = true;
    }

    // Merge into any prior partial metadata (e.g. is_global_like already
    // set by the rule-level :global propagation pass).
    let key = node_key(rel.start, rel.end);
    let entry = a.relative_selector_metadata.entry(key).or_default();
    entry.is_global = entry.is_global || meta.is_global;
    entry.is_global_like = entry.is_global_like || meta.is_global_like;
    // .scoped is set by css_prune later — preserve.

    // Recurse into pseudo-class args (e.g. `:is(.foo)`) — they may contain
    // more rules.
    for sel in &rel.selectors {
        if let SimpleSelector::PseudoClassSelector(p) = sel {
            if let Some(args) = &p.args {
                for complex in &args.children {
                    analyze_complex_selector(complex, a);
                }
            }
        }
    }
}

/// Validate `:global(...)` placement and selector-list contents.
/// Mirrors `css-analyze.js:67-116`.
fn validate_global_placement(
    complex: &ComplexSelector,
    err: &mut Option<svelte_diagnostics::CompileDiagnostic>,
) {
    if err.is_some() {
        return;
    }
    // Find the `:global(...)` relative selector, if any.
    let global_idx = complex.children.iter().position(is_global);
    if let Some(idx) = global_idx {
        let global = &complex.children[idx];
        // Get the head `:global` PseudoClassSelector.
        if let Some(SimpleSelector::PseudoClassSelector(p)) = global.selectors.first() {
            if p.args.is_some()
                && idx != 0
                && idx != complex.children.len() - 1
            {
                // Allow multiple `:global(...)` in sequence — only flag if any
                // following sibling is NOT itself global.
                for rel in complex.children.iter().skip(idx + 1) {
                    if !is_global(rel) {
                        *err = Some(svelte_diagnostics::errors::css_global_invalid_placement(
                            Some((p.start, p.end)),
                        ));
                        return;
                    }
                }
            }
        }
    }

    // `:global(...)` must not contain type/universal selectors when used in
    // a compound selector (i.e. position != 0 within its relative selector,
    // OR a sibling exists in the compound).
    for rel in &complex.children {
        for i in 0..rel.selectors.len() {
            let SimpleSelector::PseudoClassSelector(p) = &rel.selectors[i] else { continue };
            if p.name != "global" { continue }
            let Some(args) = &p.args else { continue };
            // First inner relative selector's first simple selector.
            let inner = args.children.first().and_then(|cs| cs.children.first());
            if let Some(inner_rel) = inner {
                if let Some(inner_first) = inner_rel.selectors.first() {
                    if matches!(inner_first, SimpleSelector::TypeSelector(_)) && i != 0 {
                        if err.is_none() {
                            *err = Some(
                                svelte_diagnostics::errors::css_global_invalid_selector_list(
                                    Some((p.start, p.end)),
                                ),
                            );
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// True if `simple` is `:global` with no args. Mirrors
/// `is_global_block_selector` in css-analyze.js:25-32.
fn simple_span(s: &SimpleSelector) -> (u32, u32) {
    match s {
        SimpleSelector::TypeSelector(t) => (t.start, t.end),
        SimpleSelector::ClassSelector(t) => (t.start, t.end),
        SimpleSelector::IdSelector(t) => (t.start, t.end),
        SimpleSelector::AttributeSelector(t) => (t.start, t.end),
        SimpleSelector::PseudoClassSelector(t) => (t.start, t.end),
        SimpleSelector::PseudoElementSelector(t) => (t.start, t.end),
        SimpleSelector::Nth(t) => (t.start, t.end),
        SimpleSelector::NestingSelector(t) => (t.start, t.end),
        SimpleSelector::Percentage(t) => (t.start, t.end),
    }
}

fn is_global_block_selector(simple: &SimpleSelector) -> bool {
    matches!(
        simple,
        SimpleSelector::PseudoClassSelector(p) if p.name == "global" && p.args.is_none()
    )
}

/// Mirrors `is_global` in
/// `phases/2-analyze/css/utils.js:118-132`. True if the relative selector
/// is `:global(...)` or `:global` AND nothing else in the same compound
/// scopes it.
pub fn is_global(rel: &RelativeSelector) -> bool {
    let Some(first) = rel.selectors.first() else {
        return false;
    };
    let is_first_global = matches!(
        first,
        SimpleSelector::PseudoClassSelector(p) if p.name == "global"
    );
    if !is_first_global {
        return false;
    }
    // Bare `:global` (no args) — always global.
    if let SimpleSelector::PseudoClassSelector(p) = first {
        if p.args.is_none() {
            return true;
        }
    }
    // `:global(...)` followed only by unscoped pseudo-classes / pseudo-
    // elements → still global. Otherwise → scoped (e.g. `:global(.x).y`
    // is scoped because of `.y`).
    rel.selectors.iter().all(|s| {
        is_unscoped_pseudo_class(s) || matches!(s, SimpleSelector::PseudoElementSelector(_))
    })
}

/// Mirrors `is_unscoped_pseudo_class` in utils.js:138-155.
///
/// A pseudo-class is unscoped if:
/// - It's not `:has` / `:is` / `:where` / `:not`, OR
/// - It IS one of those but all its children are themselves global.
fn is_unscoped_pseudo_class(s: &SimpleSelector) -> bool {
    let SimpleSelector::PseudoClassSelector(p) = s else {
        return false;
    };
    // First branch: non-scoping pseudo (`:hover`, `:focus`, etc.) — always
    // unscoped. For `:not`, args must be single-relative-selector form.
    let is_non_scoping = !matches!(p.name.as_ref(), "has" | "is" | "where")
        && (p.name != "not"
            || p.args.is_none()
            || p.args
                .as_ref()
                .is_some_and(|args| {
                    args.children.iter().all(|c| {
                        c.children.len() == 1 && c.children[0].selectors.len() == 1
                    })
                }));
    if is_non_scoping {
        return true;
    }
    // Second branch: `:has/:is/:where(...)` whose contents are all global —
    // counts as unscoped too. Mirrors upstream utils.js:138-156.
    match &p.args {
        None => true,
        Some(args) => args
            .children
            .iter()
            .all(|c| c.children.iter().all(is_global)),
    }
}

fn is_keyframes_name(name: &str) -> bool {
    // Strips any vendor prefix when comparing — matches `is_keyframes_node`
    // in `packages/svelte/src/compiler/phases/css.js`.
    let stripped = name
        .strip_prefix("-webkit-")
        .or_else(|| name.strip_prefix("-moz-"))
        .or_else(|| name.strip_prefix("-o-"))
        .or_else(|| name.strip_prefix("-ms-"))
        .unwrap_or(name);
    stripped == "keyframes"
}

fn node_key(start: u32, end: u32) -> NodeKey {
    (start, end)
}
