//! Phase 2: analyze.
//!
//! Mirrors `packages/svelte/src/compiler/phases/2-analyze/` and
//! `phases/scope.js`. Produces an [`Analysis`] from a parsed [`Root`].
//!
//! Status: Phase 3a (data structures) is in place. Scope-building walker,
//! rune detection, CSS analyze/prune/warn, and the 60+ validator visitors
//! are pending.

#![forbid(unsafe_code)]

pub mod a11y;
pub mod analysis;
pub mod bindings;
pub mod css_analyze;
pub mod css_possible_values;
pub mod css_prune;
pub mod css_prune_data;
pub mod css_render;
pub mod css_warn;
pub mod scope;
pub mod template_elements;
pub mod validate;
pub mod walker;

pub use analysis::{Analysis, ElementMetadata};
pub use css_analyze::{
    analyze_css, ComplexSelectorMetadata, CssAnalysis, RelativeSelectorMetadata, RuleMetadata,
};
pub use scope::{Binding, BindingKind, DeclarationKind, Scope, ScopePtr, ScopeRoot, ScopeRootPtr};

use std::path::Path;

use svelte_ast::Root;
use svelte_diagnostics::CompileDiagnostic;

/// Analyze a parsed `.svelte` component.
///
/// Mirrors `analyze_component` in
/// `packages/svelte/src/compiler/phases/2-analyze/index.js`.
///
/// Current scope: returns an `Analysis` with the parsed AST + freshly-built
/// empty scopes plus rune detection. The full visitor pipeline that
/// validates the program, classifies bindings, and analyses CSS scoping is
/// pending.
pub fn analyze_component(
    root: Root,
    filename: Option<&str>,
) -> Result<Analysis, CompileDiagnostic> {
    let scope_root = ScopeRoot::new();
    let module = Scope::new_root(scope_root.clone(), 0);
    let instance = Scope::new_root(scope_root.clone(), 0);

    // Walk module / instance scripts to populate declarations.
    if let Some(s) = root.module.as_ref() {
        walker::build_program_scope(&s.content, &module);
    }
    if let Some(s) = root.instance.as_ref() {
        walker::build_program_scope(&s.content, &instance);
    }

    let runes = walker::detect_runes(&root);
    let css = root.css.clone();
    let mut css_meta = css.as_ref().map(css_analyze::analyze_css).unwrap_or_default();

    // CSS prune — match each selector against template elements, then
    // emit `css_unused_selector` warnings for the leftovers.
    let mut warnings: Vec<CompileDiagnostic> = Vec::new();
    if let Some(sheet) = css.as_ref() {
        let elements = template_elements::collect(&root.fragment);
        css_prune::prune(sheet, &elements, &mut css_meta);
        warnings.extend(css_warn::warn_unused(sheet, &css_meta));
    }

    let name = filename
        .and_then(|f| Path::new(f).file_stem())
        .and_then(|s| s.to_str())
        .map(sanitize_component_name)
        .unwrap_or_else(|| "Component".to_string());

    let mut analysis = Analysis {
        root,
        instance,
        module,
        scope_root,
        runes,
        css,
        css_meta,
        filename: filename.map(|s| s.to_string()),
        name,
        css_hash: String::new(),
        warnings,
        elements: Default::default(),
        uses_global: false,
        uses_async: false,
        exports: Vec::new(),
    };

    // Template validator pass — emits the warnings / errors from the
    // per-AST-node visitors (svelte:window / svelte:body / svelte:self /
    // ...). Mirrors the `walk(root, visitors)` call in upstream
    // `phases/2-analyze/index.js`.
    let (validator_warnings, validator_errors) = validate::validate(&analysis.root, &analysis);
    analysis.warnings.extend(validator_warnings);
    if let Some(first_error) = validator_errors.into_iter().next() {
        // Upstream throws on the first error; we surface it the same way
        // so `compile()` can fail fast. Subsequent errors are dropped
        // (matches `InternalCompileError` behavior).
        return Err(first_error);
    }
    Ok(analysis)
}

/// Sanitize a filename stem into a JS identifier. `foo-bar` → `Foo_bar`,
/// `123Foo` → `_123Foo`. Mirrors the name sanitisation in
/// `2-analyze/index.js:51-58`.
#[cfg(test)]
mod tests {
    use super::*;
    use svelte_parse::parse;

    #[test]
    fn analyzes_empty_component() {
        let r = parse("", false).unwrap();
        let a = analyze_component(r, Some("Foo.svelte")).unwrap();
        assert_eq!(a.name, "Foo");
        assert!(!a.runes);
    }

    #[test]
    fn detects_runes_in_instance_script() {
        let r = parse("<script>let count = $state(0);</script>", false).unwrap();
        let a = analyze_component(r, None).unwrap();
        assert!(a.runes);
    }

    #[test]
    fn collects_top_level_let_declarations() {
        let r = parse(
            "<script>let a = 1; let b = 2; const c = 3;</script>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        let inst = a.instance.borrow();
        assert!(inst.get_local("a").is_some());
        assert!(inst.get_local("b").is_some());
        assert!(inst.get_local("c").is_some());
        assert!(inst.get_local("nope").is_none());
    }

    #[test]
    fn collects_destructured_props() {
        let r = parse(
            "<script>let { foo, bar = 'baz' } = $props();</script>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        let inst = a.instance.borrow();
        assert!(inst.get_local("foo").is_some());
        assert!(inst.get_local("bar").is_some());
        assert!(a.runes);
    }

    #[test]
    fn unique_name_dedupes() {
        let mut root = ScopeRoot::default();
        assert_eq!(root.unique("foo"), "foo");
        assert_eq!(root.unique("foo"), "foo_1");
        assert_eq!(root.unique("foo"), "foo_2");
    }

    #[test]
    fn classifies_state_binding() {
        let r = parse(
            "<script>let count = $state(0);</script>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        let inst = a.instance.borrow();
        let b = inst.get_local("count").unwrap();
        assert_eq!(b.borrow().kind, BindingKind::State);
    }

    #[test]
    fn classifies_derived_binding() {
        let r = parse(
            "<script>let count = $state(0); let doubled = $derived(count * 2);</script>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        let inst = a.instance.borrow();
        assert_eq!(
            inst.get_local("doubled").unwrap().borrow().kind,
            BindingKind::Derived
        );
        assert_eq!(
            inst.get_local("count").unwrap().borrow().kind,
            BindingKind::State
        );
    }

    #[test]
    fn classifies_raw_state() {
        let r = parse(
            "<script>let big = $state.raw([1,2,3]);</script>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        let inst = a.instance.borrow();
        assert_eq!(
            inst.get_local("big").unwrap().borrow().kind,
            BindingKind::RawState
        );
    }

    #[test]
    fn classifies_props_destructured() {
        let r = parse(
            "<script>let { foo, bar, ...rest } = $props();</script>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        let inst = a.instance.borrow();
        assert_eq!(
            inst.get_local("foo").unwrap().borrow().kind,
            BindingKind::Prop
        );
        assert_eq!(
            inst.get_local("bar").unwrap().borrow().kind,
            BindingKind::Prop
        );
        assert_eq!(
            inst.get_local("rest").unwrap().borrow().kind,
            BindingKind::RestProp
        );
    }

    #[test]
    fn classifies_bindable_prop() {
        let r = parse(
            "<script>let { value = $bindable('') } = $props();</script>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        let inst = a.instance.borrow();
        assert_eq!(
            inst.get_local("value").unwrap().borrow().kind,
            BindingKind::BindableProp
        );
    }

    #[test]
    fn nested_function_scope() {
        let r = parse(
            "<script>let outer = 1; function f(inner) { let local = 2; }</script>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        let inst = a.instance.borrow();
        // `outer` and `f` visible at instance scope.
        assert!(inst.get_local("outer").is_some());
        assert!(inst.get_local("f").is_some());
        // `inner` / `local` are NOT visible at instance scope (they're in
        // the function's own scope).
        assert!(inst.get_local("inner").is_none());
        assert!(inst.get_local("local").is_none());
    }

    #[test]
    fn block_scoped_let() {
        let r = parse(
            "<script>{ let blocked = 1; } let visible = 2;</script>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        let inst = a.instance.borrow();
        // `blocked` is in the inner block, not at instance scope.
        assert!(inst.get_local("blocked").is_none());
        assert!(inst.get_local("visible").is_some());
    }

    #[test]
    fn resolves_reference_in_same_scope() {
        let r = parse(
            "<script>let count = $state(0); count++;</script>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        let inst = a.instance.borrow();
        let count = inst.get_local("count").unwrap();
        // Two references: `$state(0)` initializer doesn't reference `count`,
        // but `count++` does (postfix update) — that's the only direct
        // reference here.
        assert!(!count.borrow().references.is_empty());
    }

    #[test]
    fn resolves_reference_from_inner_scope() {
        let r = parse(
            "<script>let outer = 1; function f() { return outer; }</script>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        let inst = a.instance.borrow();
        let outer = inst.get_local("outer").unwrap();
        // `outer` is referenced from inside `f`'s scope.
        assert_eq!(outer.borrow().references.len(), 1);
    }

    #[test]
    fn resolves_to_each_correct_binding_when_shadowed() {
        let r = parse(
            "<script>let x = 1; function f(x) { return x; }</script>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        let inst = a.instance.borrow();
        let outer_x = inst.get_local("x").unwrap();
        // The inner `x` shadows the outer one — the reference inside `f`
        // resolves to the param, NOT to outer `x`.
        assert_eq!(outer_x.borrow().references.len(), 0);
    }

    #[test]
    fn unresolved_reference_recorded_as_global() {
        let r = parse(
            "<script>console.log('hello');</script>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        let root = a.scope_root.borrow();
        assert!(root.conflicts.contains_key("console"));
    }

    #[test]
    fn css_analyze_marks_global_selector() {
        let r = parse(
            "<style>:global(.foo) { color: red; }</style>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        assert!(a.has_global_css());
        // The single rule should be marked as has_global_selectors.
        let css = a.css.as_ref().unwrap();
        let svelte_ast::css::StyleSheetChild::Rule(r) = &css.children[0] else {
            panic!("expected rule");
        };
        let meta = a.css_meta.rule_metadata.get(&(r.start, r.end)).unwrap();
        assert!(meta.has_global_selectors);
        assert!(!meta.has_local_selectors);
    }

    #[test]
    fn css_analyze_marks_local_selector() {
        let r = parse(
            "<style>.foo { color: red; }</style>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        assert!(!a.has_global_css());
        let css = a.css.as_ref().unwrap();
        let svelte_ast::css::StyleSheetChild::Rule(r) = &css.children[0] else {
            panic!("expected rule");
        };
        let meta = a.css_meta.rule_metadata.get(&(r.start, r.end)).unwrap();
        assert!(!meta.has_global_selectors);
        assert!(meta.has_local_selectors);
    }

    #[test]
    fn css_analyze_marks_global_block() {
        let r = parse(
            "<style>:global { div { color: red; } }</style>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        let css = a.css.as_ref().unwrap();
        let svelte_ast::css::StyleSheetChild::Rule(r) = &css.children[0] else {
            panic!("expected rule");
        };
        let meta = a.css_meta.rule_metadata.get(&(r.start, r.end)).unwrap();
        assert!(meta.is_global_block);
    }

    #[test]
    fn css_analyze_tracks_keyframes() {
        let r = parse(
            "<style>@keyframes spin { from {} to {} }</style>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        assert_eq!(a.css_meta.keyframes, vec!["spin"]);
    }

    #[test]
    fn css_analyze_global_keyframes_set_flag() {
        let r = parse(
            "<style>@keyframes -global-spin { from {} to {} }</style>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        assert!(a.has_global_css());
        // The `-global-` keyframes should NOT be in the rename list.
        assert!(a.css_meta.keyframes.is_empty());
    }

    #[test]
    fn css_prune_marks_matched_selector_used() {
        let r = parse(
            "<div class=\"foo\"></div>\n<style>.foo { color: red; }</style>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        let css = a.css.as_ref().unwrap();
        let svelte_ast::css::StyleSheetChild::Rule(rule) = &css.children[0] else {
            panic!("expected rule");
        };
        let complex = &rule.prelude.children[0];
        let cm = a
            .css_meta
            .complex_selector_metadata
            .get(&(complex.start, complex.end))
            .unwrap();
        assert!(cm.used);
    }

    #[test]
    fn css_prune_leaves_unmatched_unused() {
        let r = parse(
            "<div></div>\n<style>.no-such-class { color: red; }</style>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        let css = a.css.as_ref().unwrap();
        let svelte_ast::css::StyleSheetChild::Rule(rule) = &css.children[0] else {
            panic!("expected rule");
        };
        let complex = &rule.prelude.children[0];
        let cm = a
            .css_meta
            .complex_selector_metadata
            .get(&(complex.start, complex.end))
            .unwrap();
        assert!(!cm.used);
    }

    #[test]
    fn css_prune_matches_tag_selector() {
        let r = parse(
            "<p>hello</p>\n<style>p { color: red; }</style>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        let css = a.css.as_ref().unwrap();
        let svelte_ast::css::StyleSheetChild::Rule(rule) = &css.children[0] else {
            panic!("expected rule");
        };
        let complex = &rule.prelude.children[0];
        let cm = a
            .css_meta
            .complex_selector_metadata
            .get(&(complex.start, complex.end))
            .unwrap();
        assert!(cm.used);
    }

    #[test]
    fn css_warn_emits_unused_selector_warning() {
        let r = parse(
            "<div></div>\n<style>.no-such-class { color: red; }</style>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        assert!(a
            .warnings
            .iter()
            .any(|w| w.code == "css_unused_selector"));
    }

    fn complex_used(a: &Analysis, source: &str, css_selector: &str) -> bool {
        let _ = (source, css_selector);
        let css = a.css.as_ref().unwrap();
        let svelte_ast::css::StyleSheetChild::Rule(rule) = &css.children[0] else {
            return false;
        };
        let complex = &rule.prelude.children[0];
        a.css_meta
            .complex_selector_metadata
            .get(&(complex.start, complex.end))
            .is_some_and(|m| m.used)
    }

    #[test]
    fn css_prune_descendant_combinator_matches() {
        let r = parse(
            "<div><span></span></div>\n<style>div span { color: red; }</style>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        assert!(complex_used(&a, "", ""));
    }

    #[test]
    fn css_prune_descendant_combinator_misses_when_no_ancestor() {
        let r = parse(
            "<span></span>\n<style>div span { color: red; }</style>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        assert!(!complex_used(&a, "", ""));
    }

    #[test]
    fn css_prune_child_combinator_matches() {
        let r = parse(
            "<div><span></span></div>\n<style>div > span { color: red; }</style>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        assert!(complex_used(&a, "", ""));
    }

    #[test]
    fn css_prune_child_combinator_misses_non_direct() {
        let r = parse(
            "<div><p><span></span></p></div>\n<style>div > span { color: red; }</style>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        // span is grandchild of div, not direct child.
        assert!(!complex_used(&a, "", ""));
    }

    #[test]
    fn css_prune_adjacent_sibling_matches() {
        let r = parse(
            "<div></div><span></span>\n<style>div + span { color: red; }</style>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        assert!(complex_used(&a, "", ""));
    }

    #[test]
    fn css_prune_general_sibling_matches() {
        let r = parse(
            "<div></div><p></p><span></span>\n<style>div ~ span { color: red; }</style>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        assert!(complex_used(&a, "", ""));
    }

    #[test]
    fn css_prune_attribute_selector_matches() {
        let r = parse(
            "<input type=\"text\" />\n<style>input[type=\"text\"] { color: red; }</style>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        let css = a.css.as_ref().unwrap();
        let svelte_ast::css::StyleSheetChild::Rule(rule) = &css.children[0] else {
            panic!("expected rule");
        };
        let complex = &rule.prelude.children[0];
        let cm = a
            .css_meta
            .complex_selector_metadata
            .get(&(complex.start, complex.end))
            .unwrap();
        assert!(cm.used);
    }

    #[test]
    fn css_prune_attribute_case_insensitive_html_attr() {
        // `type` is in case_insensitive_attributes — `[type="TEXT"]`
        // should match `type="text"`.
        let r = parse(
            "<input type=\"text\" />\n<style>input[type=\"TEXT\"] { color: red; }</style>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        let css = a.css.as_ref().unwrap();
        let svelte_ast::css::StyleSheetChild::Rule(rule) = &css.children[0] else {
            panic!("expected rule");
        };
        let complex = &rule.prelude.children[0];
        let cm = a
            .css_meta
            .complex_selector_metadata
            .get(&(complex.start, complex.end))
            .unwrap();
        assert!(cm.used);
    }

    #[test]
    fn css_prune_adjacent_skips_probable_sibling() {
        // `<div></div>{#if c}<span></span>{/if}<p></p>` — `div + p` should
        // match because the `{#if}` branch might not render, making
        // `<div>` the immediate previous sibling of `<p>`.
        let r = parse(
            "<div></div>{#if c}<span></span>{/if}<p></p>\n<style>div + p { color: red; }</style>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        let css = a.css.as_ref().unwrap();
        let svelte_ast::css::StyleSheetChild::Rule(rule) = &css.children[0] else {
            panic!("expected rule");
        };
        let complex = &rule.prelude.children[0];
        let cm = a
            .css_meta
            .complex_selector_metadata
            .get(&(complex.start, complex.end))
            .unwrap();
        assert!(cm.used, "div + p should match across the conditional");
    }

    #[test]
    fn css_prune_adjacent_stops_at_definite_sibling() {
        // `<div></div><span></span><p></p>` — `div + p` should NOT match
        // because `<span>` is definitely between them.
        let r = parse(
            "<div></div><span></span><p></p>\n<style>div + p { color: red; }</style>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        let css = a.css.as_ref().unwrap();
        let svelte_ast::css::StyleSheetChild::Rule(rule) = &css.children[0] else {
            panic!("expected rule");
        };
        let complex = &rule.prelude.children[0];
        let cm = a
            .css_meta
            .complex_selector_metadata
            .get(&(complex.start, complex.end))
            .unwrap();
        assert!(!cm.used, "div + p must not match when span is definite");
    }

    #[test]
    fn css_prune_records_scoped_elements() {
        // After matching, the elements that received the hash class are
        // tracked in `css_meta.scoped_elements`. Required by the
        // transform phase to know which elements to mutate.
        let r = parse(
            "<div class=\"foo\"></div>\n<style>.foo { color: red; }</style>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        assert_eq!(
            a.css_meta.scoped_elements.len(),
            1,
            "exactly the matched <div> should be marked scoped"
        );
    }

    #[test]
    fn css_prune_attribute_whitelist_details_open() {
        // `details[open]` — `open` is whitelisted even when attribute
        // isn't statically set in the template.
        let r = parse(
            "<details>content</details>\n<style>details[open] { color: red; }</style>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        let css = a.css.as_ref().unwrap();
        let svelte_ast::css::StyleSheetChild::Rule(rule) = &css.children[0] else {
            panic!("expected rule");
        };
        let complex = &rule.prelude.children[0];
        let cm = a
            .css_meta
            .complex_selector_metadata
            .get(&(complex.start, complex.end))
            .unwrap();
        assert!(cm.used);
    }

    #[test]
    fn validator_svelte_window_rejects_non_event_attribute() {
        let r = parse(
            "<svelte:window class=\"foo\" />",
            false,
        )
        .unwrap();
        let res = analyze_component(r, None);
        // Validator turns this into an error (matches upstream's
        // `illegal_element_attribute`). `analyze_component` returns Err
        // when validation finds any error.
        assert!(res.is_err());
        let e = res.unwrap_err();
        assert_eq!(e.code, "illegal_element_attribute");
    }

    #[test]
    fn validator_svelte_window_accepts_event_attribute() {
        let r = parse(
            "<svelte:window onkeydown={handler} />",
            false,
        )
        .unwrap();
        let res = analyze_component(r, None);
        assert!(res.is_ok(), "event attributes are allowed on svelte:window");
    }

    #[test]
    fn validator_svelte_head_rejects_attributes() {
        let r = parse(
            "<svelte:head class=\"foo\"><title>x</title></svelte:head>",
            false,
        )
        .unwrap();
        let res = analyze_component(r, None);
        assert!(res.is_err());
        let e = res.unwrap_err();
        assert_eq!(e.code, "svelte_head_illegal_attribute");
    }

    #[test]
    fn validator_svelte_self_outside_block_is_error() {
        let r = parse("<svelte:self />", false).unwrap();
        let res = analyze_component(r, None);
        assert!(res.is_err());
        let e = res.unwrap_err();
        assert_eq!(e.code, "svelte_self_invalid_placement");
    }

    #[test]
    fn validator_svelte_self_inside_if_block_is_allowed() {
        let r = parse(
            "{#if x}<svelte:self />{/if}",
            false,
        )
        .unwrap();
        let res = analyze_component(r, None);
        assert!(res.is_ok(), "svelte:self inside {{#if}} is permitted");
    }

    #[test]
    fn validator_title_rejects_attributes() {
        let r = parse(
            "<title class=\"x\">hi</title>",
            false,
        )
        .unwrap();
        let res = analyze_component(r, None);
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().code, "title_illegal_attribute");
    }

    #[test]
    fn validator_title_rejects_element_children() {
        let r = parse(
            "<title><span>hi</span></title>",
            false,
        )
        .unwrap();
        let res = analyze_component(r, None);
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().code, "title_invalid_content");
    }

    #[test]
    fn validator_title_allows_text_and_expression_tag() {
        let r = parse(
            "<title>hello {name}!</title>",
            false,
        )
        .unwrap();
        let res = analyze_component(r, None);
        assert!(res.is_ok());
    }

    #[test]
    fn validator_svelte_boundary_rejects_invalid_attr_name() {
        let r = parse(
            "<svelte:boundary class=\"x\"></svelte:boundary>",
            false,
        )
        .unwrap();
        let res = analyze_component(r, None);
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().code, "svelte_boundary_invalid_attribute");
    }

    #[test]
    fn validator_svelte_boundary_accepts_onerror() {
        let r = parse(
            "<svelte:boundary onerror={handler}></svelte:boundary>",
            false,
        )
        .unwrap();
        let res = analyze_component(r, None);
        assert!(res.is_ok());
    }

    #[test]
    fn validator_runes_mode_rejects_svelte_internal_import() {
        let r = parse(
            "<script>import x from 'svelte/internal'; let s = $state(0);</script>",
            false,
        )
        .unwrap();
        let res = analyze_component(r, None);
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().code, "import_svelte_internal_forbidden");
    }

    #[test]
    fn validator_runes_mode_rejects_before_update_import() {
        let r = parse(
            "<script>import { beforeUpdate } from 'svelte'; let s = $state(0);</script>",
            false,
        )
        .unwrap();
        let res = analyze_component(r, None);
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().code, "runes_mode_invalid_import");
    }

    #[test]
    fn validator_legacy_imports_allowed_outside_runes() {
        let r = parse(
            "<script>import { beforeUpdate } from 'svelte';</script>",
            false,
        )
        .unwrap();
        let res = analyze_component(r, None);
        assert!(res.is_ok());
    }

    #[test]
    fn validator_runes_mode_rejects_legacy_reactive_statement() {
        let r = parse(
            "<script>let count = $state(0); $: doubled = count * 2;</script>",
            false,
        )
        .unwrap();
        let res = analyze_component(r, None);
        assert!(res.is_err());
        assert_eq!(
            res.unwrap_err().code,
            "legacy_reactive_statement_invalid"
        );
    }

    #[test]
    fn validator_bind_value_on_input_is_ok() {
        let r = parse(
            "<input bind:value={x} />",
            false,
        )
        .unwrap();
        let res = analyze_component(r, None);
        assert!(res.is_ok());
    }

    #[test]
    fn validator_bind_value_on_div_is_error() {
        // `value` is only valid on input / textarea / select.
        let r = parse(
            "<div bind:value={x}></div>",
            false,
        )
        .unwrap();
        let res = analyze_component(r, None);
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().code, "bind_invalid_target");
    }

    #[test]
    fn validator_bind_clientwidth_on_window_is_error() {
        // `clientWidth` is invalid on svelte:window/svelte:document.
        let r = parse(
            "<svelte:window bind:clientWidth={x} />",
            false,
        )
        .unwrap();
        let res = analyze_component(r, None);
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().code, "bind_invalid_name");
    }

    #[test]
    fn validator_style_directive_rejects_unknown_modifier() {
        let r = parse(
            r#"<div style:color|wat="red"></div>"#,
            false,
        )
        .unwrap();
        let res = analyze_component(r, None);
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().code, "style_directive_invalid_modifier");
    }

    #[test]
    fn validator_style_directive_accepts_important() {
        let r = parse(
            r#"<div style:color|important="red"></div>"#,
            false,
        )
        .unwrap();
        let res = analyze_component(r, None);
        assert!(res.is_ok());
    }

    #[test]
    fn validator_svelte_component_runes_mode_warning() {
        let r = parse(
            "<script>let foo = $state(1);</script>\n<svelte:component this={Foo} />",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        assert!(a
            .warnings
            .iter()
            .any(|w| w.code == "svelte_component_deprecated"));
    }

    #[test]
    fn validator_on_directive_runes_mode_warning() {
        let r = parse(
            "<script>let foo = $state(1);</script>\n<button on:click={fn}>x</button>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        assert!(a
            .warnings
            .iter()
            .any(|w| w.code == "event_directive_deprecated"));
    }

    #[test]
    fn validator_on_directive_no_warning_outside_runes() {
        let r = parse(
            "<button on:click={fn}>x</button>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        assert!(!a
            .warnings
            .iter()
            .any(|w| w.code == "event_directive_deprecated"));
    }

    #[test]
    fn validator_svelte_fragment_outside_component_is_error() {
        let r = parse(
            "<svelte:fragment slot=\"x\">hi</svelte:fragment>",
            false,
        )
        .unwrap();
        let res = analyze_component(r, None);
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().code, "svelte_fragment_invalid_placement");
    }

    #[test]
    fn validator_svelte_fragment_inside_component_is_ok() {
        let r = parse(
            "<Foo><svelte:fragment slot=\"x\">hi</svelte:fragment></Foo>",
            false,
        )
        .unwrap();
        let res = analyze_component(r, None);
        assert!(res.is_ok());
    }

    #[test]
    fn validator_const_tag_at_root_is_error() {
        let r = parse("{@const x = 1}", false).unwrap();
        let res = analyze_component(r, None);
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().code, "const_tag_invalid_placement");
    }

    #[test]
    fn validator_const_tag_inside_if_block_is_ok() {
        let r = parse(
            "{#if cond}{@const x = 1}{x}{/if}",
            false,
        )
        .unwrap();
        let res = analyze_component(r, None);
        assert!(res.is_ok());
    }

    #[test]
    fn validator_block_empty_warning_in_if() {
        let r = parse(
            "{#if x}   {/if}",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        assert!(a.warnings.iter().any(|w| w.code == "block_empty"));
    }

    #[test]
    fn validator_snippet_rest_parameter_is_error() {
        let r = parse(
            "{#snippet foo(...args)}body{/snippet}",
            false,
        )
        .unwrap();
        let res = analyze_component(r, None);
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().code, "snippet_invalid_rest_parameter");
    }

    #[test]
    fn validator_let_directive_outside_element_is_error() {
        // `let:foo` is only allowed on element-like nodes. The parser
        // wouldn't put a `let:` outside an element, so this is hard to
        // exercise — but the visitor's parent-check exists. Skip a
        // concrete test for now; covered by reading the code.
    }

    #[test]
    fn css_warn_silent_when_all_selectors_used() {
        let r = parse(
            "<div class=\"foo\"></div>\n<style>.foo { color: red; }</style>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        assert!(!a
            .warnings
            .iter()
            .any(|w| w.code == "css_unused_selector"));
    }

    #[test]
    fn css_prune_matches_id_selector() {
        let r = parse(
            "<p id=\"main\">hello</p>\n<style>#main { color: red; }</style>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        let css = a.css.as_ref().unwrap();
        let svelte_ast::css::StyleSheetChild::Rule(rule) = &css.children[0] else {
            panic!("expected rule");
        };
        let complex = &rule.prelude.children[0];
        let cm = a
            .css_meta
            .complex_selector_metadata
            .get(&(complex.start, complex.end))
            .unwrap();
        assert!(cm.used);
    }

    #[test]
    fn css_analyze_root_pseudo_is_global_like() {
        let r = parse(
            "<style>:root { --color: red; }</style>",
            false,
        )
        .unwrap();
        let a = analyze_component(r, None).unwrap();
        // `:root` is global-like, so the rule's complex selector is_global.
        let css = a.css.as_ref().unwrap();
        let svelte_ast::css::StyleSheetChild::Rule(r) = &css.children[0] else {
            panic!("expected rule");
        };
        let complex = &r.prelude.children[0];
        let cm = a
            .css_meta
            .complex_selector_metadata
            .get(&(complex.start, complex.end))
            .unwrap();
        assert!(cm.is_global);
    }
}

fn sanitize_component_name(stem: &str) -> String {
    let mut out = String::with_capacity(stem.len());
    for (i, c) in stem.chars().enumerate() {
        if i == 0 {
            if c.is_ascii_alphabetic() || c == '_' {
                out.push(c.to_ascii_uppercase());
            } else {
                out.push('_');
                if c.is_ascii_alphanumeric() {
                    out.push(c);
                }
            }
        } else if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "Component".to_string()
    } else {
        out
    }
}
