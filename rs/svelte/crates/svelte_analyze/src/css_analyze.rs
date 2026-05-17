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
    let mut a = CssAnalysis::default();
    for child in &stylesheet.children {
        match child {
            StyleSheetChild::Rule(rule) => analyze_rule(rule, &mut a, None),
            StyleSheetChild::Atrule(atrule) => analyze_atrule(atrule, &mut a, false),
        }
    }
    a
}

fn analyze_atrule(
    atrule: &svelte_ast::css::Atrule,
    a: &mut CssAnalysis,
    inside_global_block: bool,
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
                svelte_ast::css::BlockChild::Rule(r) => analyze_rule(r, a, None),
                svelte_ast::css::BlockChild::Atrule(at) => {
                    analyze_atrule(at, a, inside_global_block)
                }
                svelte_ast::css::BlockChild::Declaration(_) => {}
            }
        }
    }
}

fn analyze_rule(rule: &Rule, a: &mut CssAnalysis, parent_rule: Option<&Rule>) {
    let mut meta = RuleMetadata::default();

    // First pass: detect `:global { ... }` block-rule. Walks complex
    // selectors looking for a `:global` PseudoClassSelector with no args
    // at the head. Mirrors css-analyze.js:201-264. Also propagates
    // `is_global_like = true` to relative selectors that follow a `:global`
    // within the same complex selector (so `:global div` → div is_global_like).
    for complex in &rule.prelude.children {
        let mut after_global = false;
        for rel in &complex.children {
            if rel
                .selectors
                .first()
                .is_some_and(is_global_block_selector)
            {
                meta.is_global_block = true;
                after_global = true;
            } else if after_global {
                a.relative_selector_metadata
                    .entry(node_key(rel.start, rel.end))
                    .or_default()
                    .is_global_like = true;
            }
        }
    }

    // Walk selectors to populate complex / relative metadata.
    for complex in &rule.prelude.children {
        analyze_complex_selector(complex, a);
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

    a.rule_metadata.insert(node_key(rule.start, rule.end), meta);

    // Recurse into nested rules.
    for child in &rule.block.children {
        match child {
            svelte_ast::css::BlockChild::Rule(nested) => analyze_rule(nested, a, Some(rule)),
            svelte_ast::css::BlockChild::Atrule(at) => {
                analyze_atrule(at, a, meta.is_global_block)
            }
            svelte_ast::css::BlockChild::Declaration(_) => {}
        }
    }
    let _ = parent_rule;
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
                        p.name.as_str(),
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

/// True if `simple` is `:global` with no args. Mirrors
/// `is_global_block_selector` in css-analyze.js:25-32.
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
    let is_non_scoping = !matches!(p.name.as_str(), "has" | "is" | "where")
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
