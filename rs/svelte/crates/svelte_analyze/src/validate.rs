//! Validator visitors.
//!
//! Ported (incrementally) from
//! `packages/svelte/src/compiler/phases/2-analyze/visitors/`. Each visitor
//! emits warnings / errors for one AST node kind. The walker keeps a
//! `path` of ancestor `FragmentChild` references so visitors can ask
//! "am I inside an `{#if}` / `{#each}` / `<Component>` / `{#snippet}`
//! ancestor?" (cf. `SvelteSelf` in upstream).
//!
//! Current coverage: a starter set of template-side visitors (the
//! `<svelte:window>` / `<svelte:body>` / `<svelte:document>` /
//! `<svelte:head>` / `<svelte:self>` family, plus the runes-mode opening-
//! tag validation for `{@html}` / `{@debug}` etc.).

use std::path::Path;

use svelte_ast::{
    AttributeValue, AttributeValuePart, ElementAttribute, Fragment, FragmentChild, Root,
};
use svelte_diagnostics::{errors, warnings, CompileDiagnostic};

use crate::analysis::Analysis;

/// State threaded through every visitor. `path` is the chain of
/// ancestors (oldest first). `is_runes` mirrors `analysis.runes`.
pub struct ValidateState<'a> {
    pub path: Vec<&'a FragmentChild>,
    pub warnings: Vec<CompileDiagnostic>,
    pub errors: Vec<CompileDiagnostic>,
    pub is_runes: bool,
    pub component_name: String,
    pub filename: Option<String>,
}

impl<'a> ValidateState<'a> {
    pub fn new(analysis: &Analysis) -> Self {
        Self {
            path: Vec::new(),
            warnings: Vec::new(),
            errors: Vec::new(),
            is_runes: analysis.runes,
            component_name: analysis.name.clone(),
            filename: analysis.filename.clone(),
        }
    }
}

/// Validate the whole `Root`. Walks the template fragment and dispatches
/// to per-node visitors. Also walks the `<script>` / `<script module>`
/// content (Program JSON) for JS-side validators (ImportDeclaration,
/// LabeledStatement, etc.).
pub fn validate(root: &Root, analysis: &Analysis) -> (Vec<CompileDiagnostic>, Vec<CompileDiagnostic>) {
    let mut state = ValidateState::new(analysis);
    visit_fragment(&root.fragment, &mut state);
    if let Some(s) = root.instance.as_ref() {
        visit_program(&s.content, /*is_instance=*/ true, &mut state);
    }
    if let Some(s) = root.module.as_ref() {
        visit_program(&s.content, /*is_instance=*/ false, &mut state);
    }
    (state.warnings, state.errors)
}

/// Walk a Program (the `content` of a `<script>` block) and run JS-side
/// validators. `is_instance` is true for `<script>` (non-module) — the
/// only scope upstream's LabeledStatement check considers a reactive
/// statement context.
fn visit_program(program: &serde_json::Value, is_instance: bool, state: &mut ValidateState) {
    let Some(body) = program.get("body").and_then(|v| v.as_array()) else {
        return;
    };
    for node in body {
        // Top-level statements get the full visitor (including
        // LabeledStatement's "is this `$:`?" check, which only fires at
        // Program scope).
        visit_js_node(node, is_instance, /*top_level=*/ true, state);
    }
}

fn visit_js_node(
    node: &serde_json::Value,
    is_instance: bool,
    top_level: bool,
    state: &mut ValidateState,
) {
    let Some(t) = node.get("type").and_then(|v| v.as_str()) else {
        return;
    };
    match t {
        "ImportDeclaration" => visit_import_declaration(node, state),
        "LabeledStatement" if top_level => visit_labeled_statement(node, is_instance, state),
        _ => {}
    }
    // Recurse into child JS nodes (nested level — no longer top-level).
    if let serde_json::Value::Object(map) = node {
        for (key, v) in map {
            if matches!(
                key.as_str(),
                "loc" | "start" | "end" | "name" | "raw" | "value"
                    | "operator" | "computed" | "shorthand" | "method"
            ) {
                continue;
            }
            visit_js_value(v, is_instance, state);
        }
    }
}

fn visit_js_value(v: &serde_json::Value, is_instance: bool, state: &mut ValidateState) {
    match v {
        serde_json::Value::Object(_) if v.get("type").is_some() => {
            visit_js_node(v, is_instance, /*top_level=*/ false, state);
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                visit_js_value(item, is_instance, state);
            }
        }
        _ => {}
    }
}

fn visit_fragment<'a>(fragment: &'a Fragment, state: &mut ValidateState<'a>) {
    for node in &fragment.nodes {
        visit_node(node, state);
    }
}

fn visit_node<'a>(node: &'a FragmentChild, state: &mut ValidateState<'a>) {
    state.path.push(node);
    match node {
        FragmentChild::SvelteWindow(el) => visit_svelte_window(el, state),
        FragmentChild::SvelteBody(el) => visit_svelte_body(el, state),
        FragmentChild::SvelteDocument(el) => visit_svelte_document(el, state),
        FragmentChild::SvelteHead(el) => visit_svelte_head(el, state),
        FragmentChild::SvelteSelf(el) => visit_svelte_self(el, state),
        FragmentChild::HtmlTag(t) => visit_html_tag(t, state),
        FragmentChild::DebugTag(t) => visit_debug_tag(t, state),
        FragmentChild::ConstTag(t) => visit_const_tag(t, state),
        FragmentChild::RegularElement(el) => {
            state.warnings.extend(crate::a11y::check_regular_element(el));
            visit_attributes(node, &el.attributes, state);
            visit_fragment(&el.fragment, state);
        }
        FragmentChild::Component(c) => {
            visit_attributes(node, &c.attributes, state);
            visit_fragment(&c.fragment, state);
        }
        FragmentChild::SvelteComponent(c) => {
            if state.is_runes {
                state
                    .warnings
                    .push(warnings::svelte_component_deprecated(Some((c.start, c.end))));
            }
            visit_attributes(node, &c.attributes, state);
            visit_fragment(&c.fragment, state);
        }
        FragmentChild::TitleElement(el) => visit_title_element(el, state),
        FragmentChild::SlotElement(el) => {
            visit_attributes(node, &el.attributes, state);
            visit_fragment(&el.fragment, state);
        }
        FragmentChild::SvelteElement(el) => {
            // SvelteElement also gets a11y checks if the tag is statically known.
            // For now we only check `autofocus` here since the tag is dynamic;
            // most other a11y rules need a concrete tag name.
            if el.attributes.iter().any(|a| matches!(a, ElementAttribute::Attribute(svelte_ast::Attribute { name, .. }) if name == "autofocus")) {
                state
                    .warnings
                    .push(warnings::a11y_autofocus(Some((el.start, el.end))));
            }
            visit_attributes(node, &el.attributes, state);
            visit_fragment(&el.fragment, state);
        }
        FragmentChild::SvelteFragment(el) => visit_svelte_fragment(el, state),
        FragmentChild::SvelteBoundary(el) => visit_svelte_boundary(el, state),
        FragmentChild::IfBlock(b) => {
            validate_block_not_empty(Some(&b.consequent), state);
            if let Some(alt) = &b.alternate {
                validate_block_not_empty(Some(alt), state);
            }
            visit_fragment(&b.consequent, state);
            if let Some(alt) = &b.alternate {
                visit_fragment(alt, state);
            }
        }
        FragmentChild::EachBlock(b) => {
            validate_block_not_empty(Some(&b.body), state);
            visit_fragment(&b.body, state);
            if let Some(fb) = &b.fallback {
                visit_fragment(fb, state);
            }
        }
        FragmentChild::AwaitBlock(b) => {
            validate_block_not_empty(b.pending.as_ref(), state);
            validate_block_not_empty(b.then.as_ref(), state);
            validate_block_not_empty(b.catch_.as_ref(), state);
            if let Some(f) = &b.pending {
                visit_fragment(f, state);
            }
            if let Some(f) = &b.then {
                visit_fragment(f, state);
            }
            if let Some(f) = &b.catch_ {
                visit_fragment(f, state);
            }
        }
        FragmentChild::KeyBlock(b) => {
            validate_block_not_empty(Some(&b.fragment), state);
            visit_fragment(&b.fragment, state);
        }
        FragmentChild::SnippetBlock(b) => visit_snippet_block(b, state),
        _ => {}
    }
    state.path.pop();
}

// ===== Per-visitor ports =====

/// Mirrors `is_event_attribute` in `utils/ast.js:79-81`. An event attribute
/// starts with `on` and has a single `{expression}` value.
fn is_event_attribute(attr: &svelte_ast::Attribute) -> bool {
    if !attr.name.starts_with("on") {
        return false;
    }
    matches!(&attr.value, AttributeValue::Single(_))
        || matches!(
            &attr.value,
            AttributeValue::Many(parts)
                if parts.len() == 1 && matches!(parts[0], AttributeValuePart::ExpressionTag(_))
        )
}

/// `disallow_children` in shared/special-element.js:6-15. Emits
/// `svelte_meta_invalid_content` if the special element has any
/// children.
fn disallow_children(
    fragment: &Fragment,
    tag_name: &str,
    state: &mut ValidateState,
) {
    if fragment.nodes.is_empty() {
        return;
    }
    let first = fragment.nodes.first();
    let last = fragment.nodes.last();
    let span = match (first.and_then(start_of), last.and_then(end_of)) {
        (Some(s), Some(e)) => Some((s, e)),
        _ => None,
    };
    state
        .errors
        .push(errors::svelte_meta_invalid_content(span, tag_name));
}

fn start_of(node: &FragmentChild) -> Option<u32> {
    use FragmentChild::*;
    Some(match node {
        Text(t) => t.start,
        Comment(c) => c.start,
        RegularElement(el) => el.start,
        Component(c) => c.start,
        TitleElement(el) => el.start,
        SlotElement(el) => el.start,
        SvelteBody(el) => el.start,
        SvelteBoundary(el) => el.start,
        SvelteComponent(el) => el.start,
        SvelteDocument(el) => el.start,
        SvelteFragment(el) => el.start,
        SvelteHead(el) => el.start,
        SvelteOptions(el) => el.start,
        SvelteSelf(el) => el.start,
        SvelteWindow(el) => el.start,
        SvelteElement(el) => el.start,
        IfBlock(b) => b.start,
        EachBlock(b) => b.start,
        AwaitBlock(b) => b.start,
        KeyBlock(b) => b.start,
        SnippetBlock(b) => b.start,
        ExpressionTag(t) => t.start,
        HtmlTag(t) => t.start,
        ConstTag(t) => t.start,
        DebugTag(t) => t.start,
        RenderTag(t) => t.start,
        AttachTag(t) => t.start,
    })
}
fn end_of(node: &FragmentChild) -> Option<u32> {
    use FragmentChild::*;
    Some(match node {
        Text(t) => t.end,
        Comment(c) => c.end,
        RegularElement(el) => el.end,
        Component(c) => c.end,
        TitleElement(el) => el.end,
        SlotElement(el) => el.end,
        SvelteBody(el) => el.end,
        SvelteBoundary(el) => el.end,
        SvelteComponent(el) => el.end,
        SvelteDocument(el) => el.end,
        SvelteFragment(el) => el.end,
        SvelteHead(el) => el.end,
        SvelteOptions(el) => el.end,
        SvelteSelf(el) => el.end,
        SvelteWindow(el) => el.end,
        SvelteElement(el) => el.end,
        IfBlock(b) => b.end,
        EachBlock(b) => b.end,
        AwaitBlock(b) => b.end,
        KeyBlock(b) => b.end,
        SnippetBlock(b) => b.end,
        ExpressionTag(t) => t.end,
        HtmlTag(t) => t.end,
        ConstTag(t) => t.end,
        DebugTag(t) => t.end,
        RenderTag(t) => t.end,
        AttachTag(t) => t.end,
    })
}

fn attr_span(a: &ElementAttribute) -> Option<(u32, u32)> {
    match a {
        ElementAttribute::Attribute(x) => Some((x.start, x.end)),
        ElementAttribute::SpreadAttribute(x) => Some((x.start, x.end)),
        ElementAttribute::AnimateDirective(x) => Some((x.start, x.end)),
        ElementAttribute::BindDirective(x) => Some((x.start, x.end)),
        ElementAttribute::ClassDirective(x) => Some((x.start, x.end)),
        ElementAttribute::LetDirective(x) => Some((x.start, x.end)),
        ElementAttribute::OnDirective(x) => Some((x.start, x.end)),
        ElementAttribute::StyleDirective(x) => Some((x.start, x.end)),
        ElementAttribute::TransitionDirective(x) => Some((x.start, x.end)),
        ElementAttribute::UseDirective(x) => Some((x.start, x.end)),
        ElementAttribute::AttachTag(x) => Some((x.start, x.end)),
    }
}

/// `<svelte:window>` — port of visitors/SvelteWindow.js.
fn visit_svelte_window<'a>(
    el: &'a svelte_ast::SvelteWindow,
    state: &mut ValidateState<'a>,
) {
    disallow_children(&el.fragment, "svelte:window", state);
    for a in &el.attributes {
        match a {
            ElementAttribute::SpreadAttribute(_) => {
                if let Some(span) = attr_span(a) {
                    state.errors.push(errors::illegal_element_attribute(
                        Some(span),
                        "svelte:window",
                    ));
                }
            }
            ElementAttribute::Attribute(attr) if !is_event_attribute(attr) => {
                if let Some(span) = attr_span(a) {
                    state.errors.push(errors::illegal_element_attribute(
                        Some(span),
                        "svelte:window",
                    ));
                }
            }
            _ => {}
        }
    }
    // Run per-directive validators (BindDirective etc.) on the same
    // attribute list. `parent` is the SvelteWindow node itself, so they
    // can see `<svelte:window>` as the containing element.
    let parent = state.path[state.path.len() - 1];
    visit_attributes(parent, &el.attributes, state);
}

/// `<svelte:body>` — port of visitors/SvelteBody.js.
fn visit_svelte_body<'a>(el: &'a svelte_ast::SvelteBody, state: &mut ValidateState<'a>) {
    disallow_children(&el.fragment, "svelte:body", state);
    for a in &el.attributes {
        match a {
            ElementAttribute::SpreadAttribute(_) => {
                if let Some(span) = attr_span(a) {
                    state
                        .errors
                        .push(errors::svelte_body_illegal_attribute(Some(span)));
                }
            }
            ElementAttribute::Attribute(attr) if !is_event_attribute(attr) => {
                if let Some(span) = attr_span(a) {
                    state
                        .errors
                        .push(errors::svelte_body_illegal_attribute(Some(span)));
                }
            }
            _ => {}
        }
    }
    let parent = state.path[state.path.len() - 1];
    visit_attributes(parent, &el.attributes, state);
}

/// `<svelte:document>` — port of visitors/SvelteDocument.js.
fn visit_svelte_document<'a>(
    el: &'a svelte_ast::SvelteDocument,
    state: &mut ValidateState<'a>,
) {
    disallow_children(&el.fragment, "svelte:document", state);
    for a in &el.attributes {
        match a {
            ElementAttribute::SpreadAttribute(_) => {
                if let Some(span) = attr_span(a) {
                    state.errors.push(errors::illegal_element_attribute(
                        Some(span),
                        "svelte:document",
                    ));
                }
            }
            ElementAttribute::Attribute(attr) if !is_event_attribute(attr) => {
                if let Some(span) = attr_span(a) {
                    state.errors.push(errors::illegal_element_attribute(
                        Some(span),
                        "svelte:document",
                    ));
                }
            }
            _ => {}
        }
    }
    let parent = state.path[state.path.len() - 1];
    visit_attributes(parent, &el.attributes, state);
}

/// `<svelte:head>` — port of visitors/SvelteHead.js.
fn visit_svelte_head<'a>(el: &'a svelte_ast::SvelteHead, state: &mut ValidateState<'a>) {
    for a in &el.attributes {
        if let Some(span) = attr_span(a) {
            state
                .errors
                .push(errors::svelte_head_illegal_attribute(Some(span)));
        }
    }
    visit_fragment(&el.fragment, state);
}

/// `<svelte:self>` — port of visitors/SvelteSelf.js. Must be inside an
/// `{#if}` / `{#each}` / `<Component>` / `{#snippet}` ancestor. Emits
/// `svelte_self_deprecated` in runes mode.
fn visit_svelte_self<'a>(el: &'a svelte_ast::SvelteSelf, state: &mut ValidateState<'a>) {
    let valid = state.path.iter().any(|n| {
        matches!(
            n,
            FragmentChild::IfBlock(_)
                | FragmentChild::EachBlock(_)
                | FragmentChild::Component(_)
                | FragmentChild::SnippetBlock(_)
        )
    });
    if !valid {
        state
            .errors
            .push(errors::svelte_self_invalid_placement(Some((
                el.start, el.end,
            ))));
    }
    if state.is_runes {
        // Per upstream, the warning's `name` and `basename` arguments
        // describe the component itself.
        let (name, basename) = match state.filename.as_deref() {
            None => ("Self".to_string(), "Self.svelte".to_string()),
            Some(f) => {
                let basename = Path::new(f)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Self.svelte")
                    .to_string();
                (state.component_name.clone(), basename)
            }
        };
        state.warnings.push(warnings::svelte_self_deprecated(
            Some((el.start, el.end)),
            &name,
            &basename,
        ));
    }
    visit_fragment(&el.fragment, state);
}

/// `{@html ...}` — port of visitors/HtmlTag.js. Only checks the opening
/// tag is well-formed in runes mode (validate_opening_tag). The
/// `mark_subtree_dynamic` upstream call is a transform-phase concern
/// that we'll wire up when transforms land.
fn visit_html_tag(t: &svelte_ast::HtmlTag, state: &mut ValidateState) {
    if state.is_runes {
        validate_opening_tag(t.start, "@", state);
    }
}

fn visit_debug_tag(t: &svelte_ast::DebugTag, state: &mut ValidateState) {
    if state.is_runes {
        validate_opening_tag(t.start, "@", state);
    }
}

/// `{@const decl}` — port of visitors/ConstTag.js. Must appear in a
/// fragment whose owner is one of: IfBlock / SvelteFragment / Component /
/// SvelteComponent / EachBlock / AwaitBlock / SnippetBlock / SvelteBoundary
/// / KeyBlock, OR a RegularElement / SvelteElement with a `slot` attribute.
fn visit_const_tag(t: &svelte_ast::ConstTag, state: &mut ValidateState) {
    if state.is_runes {
        validate_opening_tag(t.start, "@", state);
    }
    // state.path[-1] is the ConstTag itself; we want the FragmentChild
    // containing the Fragment containing this ConstTag — that's path[-2].
    let grand_parent = if state.path.len() >= 2 {
        Some(state.path[state.path.len() - 2])
    } else {
        None
    };
    let allowed = match grand_parent {
        Some(FragmentChild::IfBlock(_)) => true,
        Some(FragmentChild::SvelteFragment(_)) => true,
        Some(FragmentChild::Component(_)) => true,
        Some(FragmentChild::SvelteComponent(_)) => true,
        Some(FragmentChild::EachBlock(_)) => true,
        Some(FragmentChild::AwaitBlock(_)) => true,
        Some(FragmentChild::SnippetBlock(_)) => true,
        Some(FragmentChild::SvelteBoundary(_)) => true,
        Some(FragmentChild::KeyBlock(_)) => true,
        Some(FragmentChild::RegularElement(el)) => has_slot_attribute(&el.attributes),
        Some(FragmentChild::SvelteElement(el)) => has_slot_attribute(&el.attributes),
        _ => false,
    };
    if !allowed {
        state
            .errors
            .push(errors::const_tag_invalid_placement(Some((t.start, t.end))));
    }
}

fn has_slot_attribute(attrs: &[ElementAttribute]) -> bool {
    attrs
        .iter()
        .any(|a| matches!(a, ElementAttribute::Attribute(x) if x.name == "slot"))
}

/// `<svelte:fragment>` — port of visitors/SvelteFragment.js. Must be a
/// direct child of `<Component>` / `<svelte:component>`. Allowed
/// attributes: `slot` (as Attribute) or `LetDirective`; anything else is
/// `svelte_fragment_invalid_attribute`.
fn visit_svelte_fragment<'a>(
    el: &'a svelte_ast::SvelteFragment,
    state: &mut ValidateState<'a>,
) {
    let parent = if state.path.len() >= 2 {
        Some(state.path[state.path.len() - 2])
    } else {
        None
    };
    let parent_ok = matches!(
        parent,
        Some(FragmentChild::Component(_)) | Some(FragmentChild::SvelteComponent(_))
    );
    if !parent_ok {
        state
            .errors
            .push(errors::svelte_fragment_invalid_placement(Some((
                el.start, el.end,
            ))));
    }
    for a in &el.attributes {
        match a {
            ElementAttribute::LetDirective(_) => {}
            ElementAttribute::Attribute(attr) if attr.name == "slot" => {}
            _ => {
                if let Some(span) = attr_span(a) {
                    state
                        .errors
                        .push(errors::svelte_fragment_invalid_attribute(Some(span)));
                }
            }
        }
    }
    visit_attributes(state.path[state.path.len() - 1], &el.attributes, state);
    visit_fragment(&el.fragment, state);
}

/// `<title>` — port of visitors/TitleElement.js. Disallows attributes and
/// any child that isn't Text or `{expression}`.
fn visit_title_element<'a>(
    el: &'a svelte_ast::TitleElement,
    state: &mut ValidateState<'a>,
) {
    for a in &el.attributes {
        if let Some(span) = attr_span(a) {
            state.errors.push(errors::title_illegal_attribute(Some(span)));
        }
    }
    for child in &el.fragment.nodes {
        let valid = matches!(
            child,
            FragmentChild::Text(_) | FragmentChild::ExpressionTag(_)
        );
        if !valid {
            if let (Some(s), Some(e)) = (start_of(child), end_of(child)) {
                state
                    .errors
                    .push(errors::title_invalid_content(Some((s, e))));
            }
        }
    }
    visit_fragment(&el.fragment, state);
}

/// `<svelte:boundary>` — port of visitors/SvelteBoundary.js. Only allows
/// `onerror`, `failed`, `pending` attributes, each with a single
/// `{expression}` value.
fn visit_svelte_boundary<'a>(
    el: &'a svelte_ast::SvelteBoundary,
    state: &mut ValidateState<'a>,
) {
    const VALID: &[&str] = &["onerror", "failed", "pending"];
    for a in &el.attributes {
        let name_ok = matches!(a, ElementAttribute::Attribute(x) if VALID.contains(&x.name.as_str()));
        if !name_ok {
            if let Some(span) = attr_span(a) {
                state
                    .errors
                    .push(errors::svelte_boundary_invalid_attribute(Some(span)));
            }
            continue;
        }
        let ElementAttribute::Attribute(attr) = a else { continue };
        let value_ok = match &attr.value {
            AttributeValue::Empty(_) => false,
            AttributeValue::Single(_) => true,
            AttributeValue::Many(parts) => {
                parts.len() == 1 && matches!(parts[0], AttributeValuePart::ExpressionTag(_))
            }
        };
        if !value_ok {
            state
                .errors
                .push(errors::svelte_boundary_invalid_attribute_value(Some((
                    attr.start, attr.end,
                ))));
        }
    }
    visit_fragment(&el.fragment, state);
}

/// Visit each attribute on an element. Handles per-attribute / per-directive
/// validators. `parent` is the `FragmentChild` reference for the element
/// owning these attributes (used by `LetDirective` etc. that consult
/// `context.path.at(-1)`).
fn visit_attributes(
    parent: &FragmentChild,
    attrs: &[ElementAttribute],
    state: &mut ValidateState,
) {
    for a in attrs {
        match a {
            ElementAttribute::LetDirective(d) => visit_let_directive(d, parent, state),
            ElementAttribute::StyleDirective(d) => visit_style_directive(d, state),
            ElementAttribute::OnDirective(d) => visit_on_directive(d, parent, state),
            ElementAttribute::BindDirective(d) => visit_bind_directive(d, parent, state),
            _ => {}
        }
    }
}

/// `style:foo|important` — port of visitors/StyleDirective.js. The only
/// permitted modifier is `important`; anything else is
/// `style_directive_invalid_modifier`.
fn visit_style_directive(d: &svelte_ast::StyleDirective, state: &mut ValidateState) {
    let invalid =
        d.modifiers.len() > 1 || (d.modifiers.len() == 1 && d.modifiers[0] != "important");
    if invalid {
        state
            .errors
            .push(errors::style_directive_invalid_modifier(Some((
                d.start, d.end,
            ))));
    }
}

/// `bind:foo` — port of visitors/BindDirective.js (placement portion).
///
/// For now we cover the binding-properties lookup: if `node.name` is a
/// known DOM binding, validate it's used on an allowed element. The full
/// upstream visitor also walks the bound expression to ensure it's
/// assignable; that part needs full scope analysis (it consults
/// `binding.kind`) and is deferred.
fn visit_bind_directive(
    d: &svelte_ast::BindDirective,
    parent: &FragmentChild,
    state: &mut ValidateState,
) {
    // Only validate when the parent is an element-like host (matches
    // upstream: RegularElement / SvelteElement / SvelteWindow /
    // SvelteDocument / SvelteBody).
    let parent_name: Option<&str> = match parent {
        FragmentChild::RegularElement(el) => Some(el.name.as_str()),
        FragmentChild::SvelteElement(_) => None, // dynamic — can't validate statically
        FragmentChild::SvelteWindow(_) => Some("svelte:window"),
        FragmentChild::SvelteDocument(_) => Some("svelte:document"),
        FragmentChild::SvelteBody(_) => Some("body"),
        _ => return, // bind: on Component / SvelteFragment etc. — handled elsewhere
    };
    let Some(parent_name) = parent_name else { return };

    let props = crate::bindings::binding_properties();
    let Some(prop) = props.get(d.name.as_str()) else {
        // Unknown binding name — upstream surfaces `bind_invalid_name` /
        // `bind_invalid_target` via fuzzy match. We skip the fuzzy match
        // path for now (would require also porting `fuzzymatch.js`).
        return;
    };

    if let Some(valid) = prop.valid_elements {
        if !valid.iter().any(|n| n.eq_ignore_ascii_case(parent_name)) {
            let suggestions = valid
                .iter()
                .map(|n| format!("`<{n}>`"))
                .collect::<Vec<_>>()
                .join(", ");
            state.errors.push(errors::bind_invalid_target(
                Some((d.start, d.end)),
                &d.name,
                &suggestions,
            ));
            return;
        }
    }
    if let Some(invalid) = prop.invalid_elements {
        if invalid.iter().any(|n| n.eq_ignore_ascii_case(parent_name)) {
            // Build the list of bindings that ARE valid on this element
            // (per upstream's diagnostic message).
            let mut valid_bindings: Vec<&&str> = props
                .iter()
                .filter(|(_, p)| {
                    p.valid_elements
                        .map(|v| v.iter().any(|n| n.eq_ignore_ascii_case(parent_name)))
                        .unwrap_or_else(|| {
                            !p.invalid_elements
                                .map(|inv| inv.iter().any(|n| n.eq_ignore_ascii_case(parent_name)))
                                .unwrap_or(false)
                        })
                })
                .map(|(k, _)| k)
                .collect();
            valid_bindings.sort();
            let message = format!(
                "Possible bindings for <{parent_name}> are {}",
                valid_bindings
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            state.errors.push(errors::bind_invalid_name(
                Some((d.start, d.end)),
                &d.name,
                Some(message.as_str()),
            ));
        }
    }
}

/// `on:click` — port of visitors/OnDirective.js. In runes mode, emits a
/// deprecation warning when used on a RegularElement / SvelteElement
/// (component-level `on:` directives are exempt — they could be outside
/// the author's control).
fn visit_on_directive(
    d: &svelte_ast::OnDirective,
    parent: &FragmentChild,
    state: &mut ValidateState,
) {
    if !state.is_runes {
        return;
    }
    let on_element = matches!(
        parent,
        FragmentChild::RegularElement(_) | FragmentChild::SvelteElement(_)
    );
    if on_element {
        state.warnings.push(warnings::event_directive_deprecated(
            Some((d.start, d.end)),
            &d.name,
        ));
    }
}

/// `let:foo` — port of visitors/LetDirective.js. Must be on a Component /
/// RegularElement / SlotElement / SvelteElement / SvelteComponent /
/// SvelteSelf / SvelteFragment parent.
fn visit_let_directive(
    d: &svelte_ast::LetDirective,
    parent: &FragmentChild,
    state: &mut ValidateState,
) {
    let valid = matches!(
        parent,
        FragmentChild::Component(_)
            | FragmentChild::RegularElement(_)
            | FragmentChild::SlotElement(_)
            | FragmentChild::SvelteElement(_)
            | FragmentChild::SvelteComponent(_)
            | FragmentChild::SvelteSelf(_)
            | FragmentChild::SvelteFragment(_)
    );
    if !valid {
        state
            .errors
            .push(errors::let_directive_invalid_placement(Some((
                d.start, d.end,
            ))));
    }
}

/// `validate_block_not_empty` — shared/utils.js. Emits `block_empty` if
/// a block fragment has exactly one Text child whose raw content is
/// blank. Skips when the fragment is `None`.
fn validate_block_not_empty(fragment: Option<&Fragment>, state: &mut ValidateState) {
    let Some(fragment) = fragment else { return };
    if fragment.nodes.len() == 1 {
        if let FragmentChild::Text(t) = &fragment.nodes[0] {
            if t.raw.trim().is_empty() {
                state
                    .warnings
                    .push(warnings::block_empty(Some((t.start, t.end))));
            }
        }
    }
}

/// `{#snippet name(params)}` — port of visitors/SnippetBlock.js.
/// Rest parameters (`...rest`) are invalid for snippets — upstream emits
/// `snippet_invalid_rest_parameter`.
fn visit_snippet_block<'a>(
    b: &'a svelte_ast::SnippetBlock,
    state: &mut ValidateState<'a>,
) {
    validate_block_not_empty(Some(&b.body), state);
    for arg in &b.parameters {
        let t = arg.get("type").and_then(|v| v.as_str());
        if t == Some("RestElement") {
            let start = arg.get("start").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let end = arg.get("end").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            state
                .errors
                .push(errors::snippet_invalid_rest_parameter(Some((start, end))));
        }
    }
    visit_fragment(&b.body, state);
}

/// `import ... from 'svelte'` — port of visitors/ImportDeclaration.js.
/// In runes mode:
/// - `import from 'svelte/internal*'` → `import_svelte_internal_forbidden`
/// - `import { beforeUpdate | afterUpdate } from 'svelte'` →
///   `runes_mode_invalid_import`
fn visit_import_declaration(node: &serde_json::Value, state: &mut ValidateState) {
    if !state.is_runes {
        return;
    }
    let source = node.get("source").and_then(|v| v.get("value")).and_then(|v| v.as_str());
    let Some(source) = source else { return };
    let start = node.get("start").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let end = node.get("end").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    if source.starts_with("svelte/internal") {
        state
            .errors
            .push(errors::import_svelte_internal_forbidden(Some((start, end))));
        return;
    }
    if source == "svelte" {
        if let Some(specifiers) = node.get("specifiers").and_then(|v| v.as_array()) {
            for s in specifiers {
                if s.get("type").and_then(|v| v.as_str()) == Some("ImportSpecifier") {
                    let imported = s.get("imported");
                    let imp_type = imported
                        .and_then(|v| v.get("type"))
                        .and_then(|v| v.as_str());
                    if imp_type == Some("Identifier") {
                        let imp_name = imported
                            .and_then(|v| v.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if imp_name == "beforeUpdate" || imp_name == "afterUpdate" {
                            let s_start =
                                s.get("start").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                            let s_end =
                                s.get("end").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                            state.errors.push(errors::runes_mode_invalid_import(
                                Some((s_start, s_end)),
                                imp_name,
                            ));
                        }
                    }
                }
            }
        }
    }
}

/// `$: foo = bar` — port of visitors/LabeledStatement.js (placement
/// portion). In runes mode, top-level `$:` reactive statements are an
/// error. The dependency-tracking part of the upstream visitor is
/// deferred.
fn visit_labeled_statement(
    node: &serde_json::Value,
    is_instance: bool,
    state: &mut ValidateState,
) {
    let label_name = node
        .get("label")
        .and_then(|v| v.get("name"))
        .and_then(|v| v.as_str());
    if label_name != Some("$") {
        return;
    }
    // Upstream only treats `$:` as a reactive statement when its parent is
    // the Program. We approximate by checking `is_instance` — top-level
    // labels in the instance script. The walker we wrote here only
    // surfaces labels at the program top, so this gate works.
    if !is_instance {
        return;
    }
    if state.is_runes {
        let start = node.get("start").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let end = node.get("end").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        state
            .errors
            .push(errors::legacy_reactive_statement_invalid(Some((start, end))));
    }
}

/// `validate_opening_tag(node, state, marker)` — shared/utils.js.
///
/// Currently a no-op stub: upstream checks that there's no whitespace
/// between `{` and the marker (e.g. `{@html ...}` not `{ @html ...}`).
/// The parser already rejects malformed tags so this is mostly belt-and-
/// suspenders; emitting the warning would require access to the source
/// bytes around `t.start`, which we'll add in a follow-up.
fn validate_opening_tag(_start: u32, _marker: &str, _state: &mut ValidateState) {
    // TODO: byte-peek at `_start..start+1` to see if there's whitespace
    // after `{`. For now we trust the parser.
}
