//! CSS scoping render — produces the transformed stylesheet text with the
//! per-component hash class applied to scoped selectors.
//!
//! Port of `packages/svelte/src/compiler/phases/3-transform/css/index.js`.
//! Uses `svelte_magic_string` for surgical edits, mirroring upstream's
//! MagicString-based approach.
//!
//! Scope: handles the core scoping (append `.svelte-HASH` to the last
//! non-`:global` selector in each ComplexSelector's last RelativeSelector,
//! strip `:global(...)` wrappers, rename `@keyframes` names). Pruning,
//! minification, source-map composition, and the full minify path are
//! incremental fills driven by failing CSS fixtures.

use svelte_ast::css::{
    Atrule, BlockChild, ComplexSelector, Declaration, RelativeSelector, Rule, SelectorList,
    SimpleSelector, StyleSheet, StyleSheetChild,
};
use svelte_magic_string::MagicString;

use crate::css_analyze::CssAnalysis;

/// Render the transformed stylesheet text. The output covers the bytes
/// `[stylesheet.content.start, stylesheet.content.end)` of `source`, with
/// scoping edits applied.
pub fn render_stylesheet(
    source: &str,
    stylesheet: &StyleSheet,
    css_meta: &CssAnalysis,
    hash: &str,
) -> String {
    render_stylesheet_with_opts(source, stylesheet, css_meta, hash, false)
}

/// Same as [`render_stylesheet`] but lets the caller force dev mode, which
/// preserves empty rules (upstream keeps them so they show up in devtools).
pub fn render_stylesheet_with_opts(
    source: &str,
    stylesheet: &StyleSheet,
    css_meta: &CssAnalysis,
    hash: &str,
    dev: bool,
) -> String {
    // Operate on a MagicString of just the content range so all edits are
    // relative-positioned and `to_string()` returns exactly the rendered CSS.
    let content_start = stylesheet.content.start as usize;
    let content_end = stylesheet.content.end as usize;
    let content = &source[content_start..content_end];
    let mut code = MagicString::new(content.to_string());
    let state = RenderState {
        hash: hash.to_string(),
        keyframes: css_meta.keyframes.clone(),
        selector_suffix: format!(".{hash}"),
        content_offset: content_start as u32,
        dev,
    };
    for child in &stylesheet.children {
        match child {
            StyleSheetChild::Rule(rule) => {
                visit_rule(rule, &mut code, &state, css_meta, false, false, false)
            }
            StyleSheetChild::Atrule(atrule) => {
                visit_atrule(atrule, &mut code, &state, css_meta, false, false, false)
            }
        }
    }
    code.to_string()
}

#[inline]
fn rel(state: &RenderState, abs: u32) -> usize {
    (abs - state.content_offset) as usize
}

struct RenderState {
    hash: String,
    #[allow(dead_code)]
    keyframes: Vec<String>,
    /// `.${hash}` — appended to scoped selectors.
    selector_suffix: String,
    /// Absolute byte offset of the stylesheet content start in the full
    /// source. Used to translate AST positions (absolute) to MagicString
    /// positions (relative to content).
    content_offset: u32,
    /// Dev mode preserves empty rules so they show up in devtools.
    dev: bool,
}

fn visit_atrule(
    atrule: &Atrule,
    code: &mut MagicString,
    state: &RenderState,
    css_meta: &CssAnalysis,
    inside_global_block: bool,
    ancestor_has_local: bool,
    is_nested: bool,
) {
    // `@keyframes`: prefix the name with `${hash}-` unless it's `-global-`.
    if is_keyframes_name(&atrule.name) {
        let abs_start = (atrule.start as usize) + atrule.name.len() + 1; // '@name'
        let start = abs_start - state.content_offset as usize;
        let bytes = code.original.as_bytes();
        let mut idx = start;
        while idx < bytes.len() && bytes[idx] == b' ' {
            idx += 1;
        }
        let prelude = atrule.prelude.trim();
        if prelude.starts_with("-global-") {
            code.remove(idx, idx + 8);
        } else if !inside_global_block {
            code.prepend_right(idx, format!("{}-", state.hash));
        }
        return;
    }
    // Recurse into block children of other at-rules (e.g. @media).
    if let Some(block) = atrule.block.as_ref() {
        for child in &block.children {
            match child {
                BlockChild::Rule(rule) => visit_rule(
                    rule,
                    code,
                    state,
                    css_meta,
                    inside_global_block,
                    ancestor_has_local,
                    is_nested,
                ),
                BlockChild::Atrule(at) => visit_atrule(
                    at,
                    code,
                    state,
                    css_meta,
                    inside_global_block,
                    ancestor_has_local,
                    is_nested,
                ),
                BlockChild::Declaration(d) => visit_declaration(d, code, state),
            }
        }
    }
}

fn visit_rule(
    rule: &Rule,
    code: &mut MagicString,
    state: &RenderState,
    css_meta: &CssAnalysis,
    inside_global_block: bool,
    ancestor_has_local: bool,
    is_nested: bool,
) {
    let key = (rule.start, rule.end);
    let meta = css_meta.rule_metadata.get(&key).copied().unwrap_or_default();
    if meta.is_global_block {
        // Full `:global { ... }` wrap: prelude is just `:global` alone.
        let is_lone_global = rule.prelude.children.len() == 1
            && rule.prelude.children[0].children.len() == 1
            && rule.prelude.children[0].children[0].selectors.len() == 1;
        if is_lone_global {
            let start = rel(state, rule.start);
            let block_start = rel(state, rule.block.start);
            let block_end = rel(state, rule.block.end);
            code.prepend_right(start, "/* ");
            code.append_left(block_start + 1, "*/");
            code.prepend_right(block_end - 1, "/*");
            code.append_left(block_end, "*/");
            for child in &rule.block.children {
                match child {
                    BlockChild::Rule(r) => {
                        visit_rule(r, code, state, css_meta, true, ancestor_has_local, true)
                    }
                    BlockChild::Atrule(at) => {
                        visit_atrule(at, code, state, css_meta, true, ancestor_has_local, true)
                    }
                    BlockChild::Declaration(d) => visit_declaration(d, code, state),
                }
            }
            return;
        }
        // Non-lone global block (e.g. `div :global { ... }`): walk each
        // ComplexSelector directly — `:global` gets stripped, `div` gets
        // scoped — but skip partial-pruning (mirrors upstream's
        // SelectorList visitor early-return when `is_in_global_block`).
        for sel in &rule.prelude.children {
            visit_complex_selector(
                sel,
                code,
                state,
                css_meta,
                inside_global_block,
                ancestor_has_local,
                is_nested,
            );
        }
        for child in &rule.block.children {
            match child {
                BlockChild::Rule(r) => {
                    visit_rule(r, code, state, css_meta, true, ancestor_has_local, true)
                }
                BlockChild::Atrule(at) => {
                    visit_atrule(at, code, state, css_meta, true, ancestor_has_local, true)
                }
                BlockChild::Declaration(d) => visit_declaration(d, code, state),
            }
        }
        return;
    }

    // Empty rule (no Declarations and no used non-empty inner rules) →
    // `/* (empty) ... */` wrapper. Upstream's `Rule` visitor at
    // packages/svelte/src/compiler/phases/3-transform/css/index.js:146.
    // Dev mode keeps empty rules so they show up in devtools.
    if !state.dev && !inside_global_block && is_empty_rule(rule, css_meta, inside_global_block) {
        let start = rel(state, rule.start);
        let end = rel(state, rule.end);
        code.prepend_right(start, "/* (empty) ");
        code.append_left(end, "*/");
        escape_comment_close(rule, code, state);
        return;
    }

    // Unused rule (no used selectors) → `/* (unused) ... */` wrapper.
    if !inside_global_block && !is_rule_used(rule, css_meta) {
        let start = rel(state, rule.start);
        let end = rel(state, rule.end);
        code.prepend_right(start, "/* (unused) ");
        code.append_left(end, "*/");
        escape_comment_close(rule, code, state);
        return;
    }

    // Walk the selector list, scoping each non-global complex selector.
    visit_selector_list(
        &rule.prelude,
        code,
        state,
        css_meta,
        inside_global_block,
        ancestor_has_local,
        is_nested,
    );
    // For descendants, propagate "has_local_selectors" up the chain.
    let self_has_local = rule_has_local_selectors(rule, css_meta);
    let child_has_local = ancestor_has_local || self_has_local;
    // Recurse into the block.
    for child in &rule.block.children {
        match child {
            BlockChild::Rule(r) => visit_rule(
                r,
                code,
                state,
                css_meta,
                inside_global_block,
                child_has_local,
                true,
            ),
            BlockChild::Atrule(at) => visit_atrule(
                at,
                code,
                state,
                css_meta,
                inside_global_block,
                child_has_local,
                true,
            ),
            BlockChild::Declaration(d) => visit_declaration(d, code, state),
        }
    }
}

/// Mirrors upstream's `node.metadata.has_local_selectors` set during analyze
/// — true if any ComplexSelector in the prelude is NOT marked global.
fn rule_has_local_selectors(rule: &Rule, css_meta: &CssAnalysis) -> bool {
    for sel in &rule.prelude.children {
        let m = css_meta
            .complex_selector_metadata
            .get(&(sel.start, sel.end))
            .copied()
            .unwrap_or_default();
        if !m.is_global {
            return true;
        }
    }
    false
}

/// Mirrors upstream `is_empty` in
/// packages/svelte/src/compiler/phases/3-transform/css/index.js:424.
fn is_empty_rule(rule: &Rule, css_meta: &CssAnalysis, in_global_block: bool) -> bool {
    let key = (rule.start, rule.end);
    let meta = css_meta.rule_metadata.get(&key).copied().unwrap_or_default();
    if meta.is_global_block {
        return rule.block.children.is_empty();
    }
    for child in &rule.block.children {
        match child {
            BlockChild::Declaration(_) => return false,
            BlockChild::Rule(child_rule) => {
                if (is_rule_used(child_rule, css_meta) || in_global_block)
                    && !is_empty_rule(child_rule, css_meta, in_global_block)
                {
                    return false;
                }
            }
            BlockChild::Atrule(at) => match at.block.as_ref() {
                None => return false,
                Some(b) if !b.children.is_empty() => return false,
                _ => {}
            },
        }
    }
    true
}

fn is_rule_used(rule: &Rule, css_meta: &CssAnalysis) -> bool {
    for selector in &rule.prelude.children {
        let key = (selector.start, selector.end);
        let m = css_meta
            .complex_selector_metadata
            .get(&key)
            .copied()
            .unwrap_or_default();
        if m.used || m.is_global {
            return true;
        }
    }
    false
}

fn visit_selector_list(
    list: &SelectorList,
    code: &mut MagicString,
    state: &RenderState,
    css_meta: &CssAnalysis,
    inside_global_block: bool,
    initial_bumped: bool,
    is_nested: bool,
) {
    // Wrap unused selectors in the list with `/* (unused) */`. Mirrors the
    // SelectorList visitor at
    // packages/svelte/src/compiler/phases/3-transform/css/index.js:198-258.
    if !inside_global_block && !list.children.is_empty() {
        let raw = code.original.clone();
        let bytes = raw.as_bytes();
        let children = &list.children;
        let first_start = rel(state, children[0].start);
        let mut prune_start: usize = first_start;
        let mut last: usize = first_start;
        let mut pruning = false;
        let mut has_previous_used = false;
        for (i, sel) in children.iter().enumerate() {
            let used = is_complex_used(sel, css_meta);
            if used == pruning {
                if pruning {
                    // transition from unused → used: find the comma before this
                    // selector and append `*/` after it.
                    let mut k = rel(state, sel.start);
                    while k > 0 && bytes.get(k).copied() != Some(b',') {
                        k -= 1;
                    }
                    let insert_at = if has_previous_used { k } else { k + 1 };
                    code.append_right(insert_at, "*/");
                } else {
                    // transition from used → unused: open comment before this
                    // selector.
                    if i == 0 {
                        code.prepend_right(rel(state, sel.start), "/* (unused) ");
                    } else {
                        code.overwrite(last, rel(state, sel.start), " /* (unused) ");
                    }
                }
                pruning = !pruning;
                let _ = prune_start;
                prune_start = if i == 0 { rel(state, sel.start) } else { last };
            }
            if !pruning && used {
                has_previous_used = true;
            }
            last = rel(state, sel.end);
        }
        if pruning {
            code.append_left(last, "*/");
        }
    }

    for selector in &list.children {
        if is_complex_used(selector, css_meta) {
            visit_complex_selector(
                selector,
                code,
                state,
                css_meta,
                inside_global_block,
                initial_bumped,
                is_nested,
            );
        }
    }
}

fn is_complex_used(complex: &ComplexSelector, css_meta: &CssAnalysis) -> bool {
    let key = (complex.start, complex.end);
    let m = css_meta
        .complex_selector_metadata
        .get(&key)
        .copied()
        .unwrap_or_default();
    m.used || m.is_global
}

fn visit_complex_selector(
    complex: &ComplexSelector,
    code: &mut MagicString,
    state: &RenderState,
    css_meta: &CssAnalysis,
    inside_global_block: bool,
    initial_bumped: bool,
    is_nested: bool,
) {
    let key = (complex.start, complex.end);
    let meta = css_meta
        .complex_selector_metadata
        .get(&key)
        .copied()
        .unwrap_or_default();
    if meta.is_global || inside_global_block {
        // Strip `:global(...)` wrappers from each relative selector, then
        // recurse into any `:is/:has/:not/:where` inner selectors. When the
        // outer rule is nested AND the relative selector has no combinator,
        // append `&` after the `:global` removal so `div { :global.x }`
        // becomes `div { &.x }` (we prepend at end so it lands just before
        // the next surviving char).
        for rsel in &complex.children {
            unwrap_global_pseudo(rsel, code, state);
            if is_nested && rsel.combinator.is_none() {
                if let Some(SimpleSelector::PseudoClassSelector(p)) = rsel.selectors.first() {
                    if p.name == "global" && p.args.is_none() {
                        code.prepend_right(rel(state, p.end), "&");
                    }
                }
            }
            for s in &rsel.selectors {
                visit_inner_pseudo(s, code, state, css_meta, inside_global_block, &mut true);
            }
        }
        return;
    }
    // Apply scoping to scoped RelativeSelectors. The FIRST gets `.HASH`
    // (bumping specificity), subsequent ones get `:where(.HASH)` (no bump).
    // Matches upstream's specificity rule.
    let mut bumped = initial_bumped;
    for rsel in &complex.children {
        let rkey = (rsel.start, rsel.end);
        let rmeta = css_meta
            .relative_selector_metadata
            .get(&rkey)
            .copied()
            .unwrap_or_default();
        if rmeta.is_global || rmeta.is_global_like {
            unwrap_global_pseudo(rsel, code, state);
            for s in &rsel.selectors {
                visit_inner_pseudo(s, code, state, css_meta, inside_global_block, &mut bumped);
            }
            continue;
        }
        // Mid-compound `:global(...)` — unwrap inline (e.g.
        // `div:global(.blue)` → `div.blue`). Upstream's "else" branch in
        // index.js:311-318.
        for s in &rsel.selectors {
            if let SimpleSelector::PseudoClassSelector(p) = s {
                if p.name == "global" {
                    unwrap_one_global(p, code, state);
                }
            }
        }
        // NOTE: at the top level we don't gate on rmeta.scoped because our
        // css_prune doesn't classify every case correctly yet. Inner
        // recursion (visit_inner_complex) does gate on scoped, which is what
        // `:not(.foo)`-style filters actually need.
        // Skip standalone `:is(...)` / `:where(...)` / `&` selectors — the
        // inner selectors get scoped via recursion.
        if rsel.selectors.len() == 1 {
            match &rsel.selectors[0] {
                SimpleSelector::PseudoClassSelector(p)
                    if p.name == "is" || p.name == "where" =>
                {
                    visit_inner_pseudo(
                        &rsel.selectors[0],
                        code,
                        state,
                        css_meta,
                        inside_global_block,
                        &mut bumped,
                    );
                    continue;
                }
                SimpleSelector::NestingSelector(_) => continue,
                _ => {}
            }
        }
        // Skip relative selectors that contain a NestingSelector anywhere.
        if rsel
            .selectors
            .iter()
            .any(|s| matches!(s, SimpleSelector::NestingSelector(_)))
        {
            for s in &rsel.selectors {
                visit_inner_pseudo(s, code, state, css_meta, inside_global_block, &mut bumped);
            }
            continue;
        }
        // Walk backwards: the modifier goes after the last non-pseudo simple
        // selector. If a TypeSelector named `*` is hit, replace it with the
        // modifier. If only pseudo-class / pseudo-element selectors exist,
        // prepend before index 0 (skipping `:root`/`:host`).
        let modifier = if bumped {
            format!(":where(.{})", state.hash)
        } else {
            state.selector_suffix.clone()
        };
        let mut applied = false;
        let selectors = &rsel.selectors;
        for i in (0..selectors.len()).rev() {
            let sel = &selectors[i];
            match sel {
                SimpleSelector::PseudoElementSelector(p) => {
                    if i == 0 {
                        code.prepend_right(rel(state, p.start), modifier.clone());
                        applied = true;
                        break;
                    }
                    // walk past
                }
                SimpleSelector::PseudoClassSelector(p) => {
                    if p.name == "root" || p.name == "host" {
                        // skip — never modify these
                        if i == 0 {
                            applied = true;
                            break;
                        }
                        continue;
                    }
                    if i == 0 {
                        code.prepend_right(rel(state, p.start), modifier.clone());
                        applied = true;
                        break;
                    }
                    // walk past
                }
                SimpleSelector::TypeSelector(t) => {
                    if t.name == "*" {
                        code.overwrite(
                            rel(state, t.start),
                            rel(state, t.end),
                            modifier.clone(),
                        );
                    } else {
                        code.append_left(rel(state, t.end), modifier.clone());
                    }
                    applied = true;
                    break;
                }
                SimpleSelector::ClassSelector(c) => {
                    code.append_left(rel(state, c.end), modifier.clone());
                    applied = true;
                    break;
                }
                SimpleSelector::IdSelector(id) => {
                    code.append_left(rel(state, id.end), modifier.clone());
                    applied = true;
                    break;
                }
                SimpleSelector::AttributeSelector(a) => {
                    code.append_left(rel(state, a.end), modifier.clone());
                    applied = true;
                    break;
                }
                SimpleSelector::NestingSelector(n) => {
                    code.append_left(rel(state, n.end), modifier.clone());
                    applied = true;
                    break;
                }
                SimpleSelector::Percentage(_) | SimpleSelector::Nth(_) => {
                    // shouldn't appear in normal selector position
                }
            }
        }
        if applied {
            bumped = true;
        }
        // Recurse into any `:is/:has/:not/:where` pseudo-classes after
        // applying outer scoping so inner modifier reflects new bumped state.
        for s in &rsel.selectors {
            visit_inner_pseudo(s, code, state, css_meta, inside_global_block, &mut bumped);
        }
    }
}

/// If `selector` is one of `:is(...)`, `:where(...)`, `:has(...)`,
/// `:not(...)`, recurse into its inner SelectorList and apply scoping with
/// the carry-over bumped state. Upstream's `PseudoClassSelector` visitor.
fn visit_inner_pseudo(
    selector: &SimpleSelector,
    code: &mut MagicString,
    state: &RenderState,
    css_meta: &CssAnalysis,
    inside_global_block: bool,
    bumped: &mut bool,
) {
    let SimpleSelector::PseudoClassSelector(p) = selector else {
        return;
    };
    if p.name != "is" && p.name != "where" && p.name != "has" && p.name != "not" {
        return;
    }
    let Some(list) = p.args.as_ref() else { return };
    // Each ComplexSelector inside the args shares the outer specificity state.
    for inner in &list.children {
        let saved = *bumped;
        visit_inner_complex(inner, code, state, css_meta, inside_global_block, bumped);
        // Restore bumped: upstream restores via `before_bumped` after the
        // outer ComplexSelector visit completes. But the inner SelectorList
        // visitor itself doesn't reset between siblings inside an args list
        // — each `:is(a, b)` complex sibling can independently bump.
        if !saved && *bumped {
            // keep bumped for siblings — matches upstream's shared
            // `specificity` object.
        }
    }
}

/// Mirror of `visit_complex_selector` but for the inner-pseudo case where we
/// already carry a `bumped` flag (specificity state inherits from the outer
/// selector).
fn visit_inner_complex(
    complex: &ComplexSelector,
    code: &mut MagicString,
    state: &RenderState,
    css_meta: &CssAnalysis,
    inside_global_block: bool,
    bumped: &mut bool,
) {
    let key = (complex.start, complex.end);
    let meta = css_meta
        .complex_selector_metadata
        .get(&key)
        .copied()
        .unwrap_or_default();
    if meta.is_global || inside_global_block {
        for rsel in &complex.children {
            unwrap_global_pseudo(rsel, code, state);
            for s in &rsel.selectors {
                visit_inner_pseudo(s, code, state, css_meta, inside_global_block, bumped);
            }
        }
        return;
    }
    for rsel in &complex.children {
        let rkey = (rsel.start, rsel.end);
        let rmeta = css_meta
            .relative_selector_metadata
            .get(&rkey)
            .copied()
            .unwrap_or_default();
        if rmeta.is_global || rmeta.is_global_like {
            unwrap_global_pseudo(rsel, code, state);
            for s in &rsel.selectors {
                visit_inner_pseudo(s, code, state, css_meta, inside_global_block, bumped);
            }
            continue;
        }
        if !rmeta.scoped {
            // Unwrap mid-compound `:global(...)` even if not scoped.
            for s in &rsel.selectors {
                if let SimpleSelector::PseudoClassSelector(p) = s {
                    if p.name == "global" {
                        unwrap_one_global(p, code, state);
                    }
                }
            }
            for s in &rsel.selectors {
                visit_inner_pseudo(s, code, state, css_meta, inside_global_block, bumped);
            }
            continue;
        }
        // Mid-compound `:global(...)` unwrap.
        for s in &rsel.selectors {
            if let SimpleSelector::PseudoClassSelector(p) = s {
                if p.name == "global" {
                    unwrap_one_global(p, code, state);
                }
            }
        }
        if rsel.selectors.len() == 1 {
            match &rsel.selectors[0] {
                SimpleSelector::PseudoClassSelector(p)
                    if p.name == "is" || p.name == "where" =>
                {
                    visit_inner_pseudo(
                        &rsel.selectors[0],
                        code,
                        state,
                        css_meta,
                        inside_global_block,
                        bumped,
                    );
                    continue;
                }
                SimpleSelector::NestingSelector(_) => continue,
                _ => {}
            }
        }
        if rsel
            .selectors
            .iter()
            .any(|s| matches!(s, SimpleSelector::NestingSelector(_)))
        {
            for s in &rsel.selectors {
                visit_inner_pseudo(s, code, state, css_meta, inside_global_block, bumped);
            }
            continue;
        }
        let modifier = if *bumped {
            format!(":where(.{})", state.hash)
        } else {
            state.selector_suffix.clone()
        };
        let mut applied = false;
        let selectors = &rsel.selectors;
        for i in (0..selectors.len()).rev() {
            let sel = &selectors[i];
            match sel {
                SimpleSelector::PseudoElementSelector(p) => {
                    if i == 0 {
                        code.prepend_right(rel(state, p.start), modifier.clone());
                        applied = true;
                        break;
                    }
                }
                SimpleSelector::PseudoClassSelector(p) => {
                    if p.name == "root" || p.name == "host" {
                        if i == 0 {
                            applied = true;
                            break;
                        }
                        continue;
                    }
                    if i == 0 {
                        code.prepend_right(rel(state, p.start), modifier.clone());
                        applied = true;
                        break;
                    }
                }
                SimpleSelector::TypeSelector(t) => {
                    if t.name == "*" {
                        code.overwrite(
                            rel(state, t.start),
                            rel(state, t.end),
                            modifier.clone(),
                        );
                    } else {
                        code.append_left(rel(state, t.end), modifier.clone());
                    }
                    applied = true;
                    break;
                }
                SimpleSelector::ClassSelector(c) => {
                    code.append_left(rel(state, c.end), modifier.clone());
                    applied = true;
                    break;
                }
                SimpleSelector::IdSelector(id) => {
                    code.append_left(rel(state, id.end), modifier.clone());
                    applied = true;
                    break;
                }
                SimpleSelector::AttributeSelector(a) => {
                    code.append_left(rel(state, a.end), modifier.clone());
                    applied = true;
                    break;
                }
                SimpleSelector::NestingSelector(n) => {
                    code.append_left(rel(state, n.end), modifier.clone());
                    applied = true;
                    break;
                }
                SimpleSelector::Percentage(_) | SimpleSelector::Nth(_) => {}
            }
        }
        if applied {
            *bumped = true;
        }
        for s in &rsel.selectors {
            visit_inner_pseudo(s, code, state, css_meta, inside_global_block, bumped);
        }
    }
}

/// Replace a `:global(...)` pseudo-class wrapper with its inner selectors
/// (or `:global` bare with nothing). For bare `:global` with descendant
/// combinator, also strips the preceding whitespace (so `div :global.x`
/// becomes `div.x`).
fn unwrap_global_pseudo(rsel: &RelativeSelector, code: &mut MagicString, state: &RenderState) {
    let combinator_is_descendant =
        rsel.combinator.as_ref().is_some_and(|c| c.name == " ");
    for s in &rsel.selectors {
        if let SimpleSelector::PseudoClassSelector(p) = s {
            if p.name == "global" {
                unwrap_one_global_with_combinator(p, code, state, combinator_is_descendant);
            }
        }
    }
}

/// Unwrap one `:global(...)` / bare `:global` pseudo-class. Used by both
/// top-level and mid-compound unwrapping paths.
fn unwrap_one_global(
    p: &svelte_ast::css::PseudoClassSelector,
    code: &mut MagicString,
    state: &RenderState,
) {
    unwrap_one_global_with_combinator(p, code, state, false);
}

fn unwrap_one_global_with_combinator(
    p: &svelte_ast::css::PseudoClassSelector,
    code: &mut MagicString,
    state: &RenderState,
    combinator_is_descendant: bool,
) {
    let start = rel(state, p.start);
    let end = rel(state, p.end);
    if p.args.is_some() {
        let raw = code.original.clone();
        if let Some(open) = raw[start..end].find('(') {
            let open_idx = start + open;
            let close_idx = end - 1;
            code.remove(start, open_idx + 1);
            code.remove(close_idx, close_idx + 1);
        }
    } else {
        // Bare `:global`. For descendant combinator, walk back over
        // whitespace so `div :global.x` becomes `div.x`. Mirrors upstream
        // `remove_global_pseudo_class` at index.js:392-401.
        let mut s = start;
        if combinator_is_descendant {
            let bytes = code.original.as_bytes();
            while s > 0 {
                let ch = bytes[s - 1] as char;
                if ch == ' ' || ch == '\t' || ch == '\n' || ch == '\r' {
                    s -= 1;
                } else {
                    break;
                }
            }
        }
        code.remove(s, end);
    }
}

/// Escape any `*/` that appears inside the rule's source so the wrapping
/// `/* (unused) */ / `/* (empty) */` comments don't terminate early. Mirrors
/// `escape_comment_close` in
/// packages/svelte/src/compiler/phases/3-transform/css/index.js:458.
fn escape_comment_close(rule: &Rule, code: &mut MagicString, state: &RenderState) {
    let raw = code.original.clone();
    let bytes = raw.as_bytes();
    let start = rel(state, rule.start);
    let end = rel(state, rule.end);
    let mut escaped = false;
    let mut in_comment = false;
    let mut i = start;
    while i < end && i < bytes.len() {
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        let ch = bytes[i] as char;
        if in_comment {
            if ch == '*' && i + 1 < bytes.len() && bytes[i + 1] as char == '/' {
                code.prepend_right(i + 1, "\\");
                i += 1;
                in_comment = false;
            }
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '/' && i + 1 < bytes.len() && bytes[i + 1] as char == '*' {
            in_comment = true;
            i += 1;
        }
        i += 1;
    }
}

fn is_keyframes_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "keyframes"
        || n == "-webkit-keyframes"
        || n == "-moz-keyframes"
        || n == "-o-keyframes"
        || n == "-ms-keyframes"
}

/// Strip a CSS browser prefix (`-webkit-`, `-moz-`, `-o-`, `-ms-`).
fn remove_css_prefix(name: &str) -> &str {
    for p in ["-webkit-", "-moz-", "-o-", "-ms-"] {
        if let Some(rest) = name.strip_prefix(p) {
            return rest;
        }
    }
    name
}

/// CSS name boundary characters from upstream regex `^[\s,;}]$`.
fn is_css_name_boundary(ch: char) -> bool {
    ch == ' ' || ch == '\t' || ch == '\n' || ch == '\r' || ch == ',' || ch == ';' || ch == '}'
}

/// Walk an `animation` / `animation-name` declaration value and prepend
/// `${hash}-` to any token that matches a declared `@keyframes` name. Mirrors
/// `Declaration` visitor in upstream `packages/svelte/src/compiler/phases/3-transform/css/index.js`.
fn visit_declaration(decl: &Declaration, code: &mut MagicString, state: &RenderState) {
    let property_lower = decl.property.to_ascii_lowercase();
    let property = remove_css_prefix(&property_lower);
    if property != "animation" && property != "animation-name" {
        return;
    }
    let raw = code.original.clone();
    let bytes = raw.as_bytes();
    // `index` is the position right after the `:` separator.
    let decl_start = (decl.start as usize) - state.content_offset as usize;
    let mut index = decl_start + decl.property.len() + 1;
    let mut name = String::new();
    while index < bytes.len() {
        let ch = bytes[index] as char;
        if is_css_name_boundary(ch) {
            if state.keyframes.iter().any(|k| k == &name) {
                let start = index - name.len();
                code.prepend_right(start, format!("{}-", state.hash));
            }
            if ch == ';' || ch == '}' {
                break;
            }
            name.clear();
        } else {
            name.push(ch);
        }
        index += 1;
    }
}
