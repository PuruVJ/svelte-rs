//! CSS warn pass.
//!
//! Ported from
//! `packages/svelte/src/compiler/phases/2-analyze/css/css-warn.js`.
//!
//! After CSS prune has marked each ComplexSelector as used / unused, emit a
//! `css_unused_selector` warning for every selector that didn't match any
//! template element. Skips the prelude of `:is(...)` / `:where(...)` (those
//! are checked by their wrapping ComplexSelector) and the contents of
//! `@keyframes` (atrule preludes are exempt).

use svelte_ast::css::{ComplexSelector, Rule, SimpleSelector, StyleSheet, StyleSheetChild};
use svelte_diagnostics::{warnings, CompileDiagnostic};

use crate::css_analyze::CssAnalysis;

/// Walk the stylesheet and return one `css_unused_selector` warning per
/// unused ComplexSelector.
pub fn warn_unused(stylesheet: &StyleSheet, css_meta: &CssAnalysis) -> Vec<CompileDiagnostic> {
    let mut out = Vec::new();
    for child in &stylesheet.children {
        match child {
            StyleSheetChild::Rule(rule) => walk_rule(rule, stylesheet, css_meta, &mut out),
            StyleSheetChild::Atrule(at) => walk_atrule(at, stylesheet, css_meta, &mut out),
        }
    }
    out
}

fn walk_rule(
    rule: &Rule,
    sheet: &StyleSheet,
    css_meta: &CssAnalysis,
    out: &mut Vec<CompileDiagnostic>,
) {
    let rule_meta = css_meta
        .rule_metadata
        .get(&(rule.start, rule.end))
        .copied()
        .unwrap_or_default();

    // `:global { ... }` block: skip prelude warnings, scope-check the
    // block content instead. The body of the global block is unscoped, so
    // its nested rules are also exempt.
    if !rule_meta.is_global_block {
        for complex in &rule.prelude.children {
            check_complex(complex, sheet, css_meta, out);
        }
    }

    for child in &rule.block.children {
        match child {
            svelte_ast::css::BlockChild::Rule(nested) => walk_rule(nested, sheet, css_meta, out),
            svelte_ast::css::BlockChild::Atrule(at) => walk_atrule(at, sheet, css_meta, out),
            svelte_ast::css::BlockChild::Declaration(_) => {}
        }
    }
}

fn walk_atrule(
    atrule: &svelte_ast::css::Atrule,
    sheet: &StyleSheet,
    css_meta: &CssAnalysis,
    out: &mut Vec<CompileDiagnostic>,
) {
    // `@keyframes` preludes aren't selectors — don't warn about them.
    if is_keyframes_name(&atrule.name) {
        return;
    }
    if let Some(block) = &atrule.block {
        for child in &block.children {
            match child {
                svelte_ast::css::BlockChild::Rule(r) => walk_rule(r, sheet, css_meta, out),
                svelte_ast::css::BlockChild::Atrule(at) => walk_atrule(at, sheet, css_meta, out),
                svelte_ast::css::BlockChild::Declaration(_) => {}
            }
        }
    }
}

fn check_complex(
    complex: &ComplexSelector,
    sheet: &StyleSheet,
    css_meta: &CssAnalysis,
    out: &mut Vec<CompileDiagnostic>,
) {
    let key = (complex.start, complex.end);
    let used = css_meta
        .complex_selector_metadata
        .get(&key)
        .is_some_and(|m| m.used);
    if !used {
        // Slice the source-of-truth `content.styles` for the selector's
        // text. The CSS content has its own coordinate frame (relative to
        // the `<style>` body start), so adjust accordingly.
        let content = &sheet.content;
        let abs_start = complex.start as usize;
        let abs_end = complex.end as usize;
        let content_start = content.start as usize;
        let content_end = content_start + content.styles.len();
        let text = if abs_start >= content_start && abs_end <= content_end {
            &content.styles[abs_start - content_start..abs_end - content_start]
        } else {
            ""
        };
        out.push(warnings::css_unused_selector(
            Some((complex.start, complex.end)),
            text,
        ));
    }

    // Recurse into pseudo-class args (`:is(...)`, `:where(...)`, etc.).
    for rel in &complex.children {
        for s in &rel.selectors {
            if let SimpleSelector::PseudoClassSelector(p) = s {
                if matches!(p.name.as_str(), "is" | "where" | "not" | "has") {
                    if let Some(args) = &p.args {
                        for nested in &args.children {
                            check_complex(nested, sheet, css_meta, out);
                        }
                    }
                }
            }
        }
    }
}

fn is_keyframes_name(name: &str) -> bool {
    let stripped = name
        .strip_prefix("-webkit-")
        .or_else(|| name.strip_prefix("-moz-"))
        .or_else(|| name.strip_prefix("-o-"))
        .or_else(|| name.strip_prefix("-ms-"))
        .unwrap_or(name);
    stripped == "keyframes"
}
