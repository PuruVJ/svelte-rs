//! Element parser.
//!
//! Ported (partially) from `packages/svelte/src/compiler/phases/1-parse/state/element.js`.
//! Current coverage:
//! - Open/close tags: `<div>...</div>`
//! - Self-closing: `<br/>`
//! - Void elements: `<br>`, `<input>`, etc. — no closing tag required.
//! - Nested elements (recursive fragment parsing).
//! - Bare attributes: `<input disabled>`
//! - Double/single-quoted string attributes: `<div class="x">`, `<div class='x'>`
//!
//! Not yet covered (deferred to follow-ups):
//! - Component invocation (uppercase tag names → `Component`).
//! - `svelte:*` meta tags → `SvelteBody/Boundary/Component/...`.
//! - Expression attributes: `name={expr}`, `{...spread}`, directives
//!   (`bind:`, `on:`, `use:`, `transition:`, `in:`, `out:`, `animate:`,
//!   `class:`, `let:`, `style:`).
//! - Unquoted attribute values.
//! - HTML entity decoding inside attribute values (currently pass-through).
//! - Auto-closing logic (`<p><p>` closes the first one).

use serde_json::{json, Value};
use std::borrow::Cow;

use svelte_ast::{
    AnimateDirective,  AttachTag,  Attribute, 
    AttributeValue, AttributeValuePart, BindDirective,  ClassDirective,
     ElementAttribute, ExpressionTag, 
    Fragment, FragmentChild,  LetDirective,  OnDirective,
     RegularElement,  SlotElement,  
    SourceLocation, StyleDirective,  SvelteBody,  
    SvelteBoundary,   SvelteDocument, 
     SvelteFragment,   SvelteHead,
      SvelteOptionsRaw,  
    SvelteSelf,   SvelteWindow,  
    Text,  TitleElement,   TransitionDirective,
     UseDirective, 
};
use svelte_diagnostics::{errors, CompileDiagnostic};

use crate::parser::Parser;
use crate::utils::element_names::{is_svelte_meta_name, is_valid_tag_name, is_void};

/// Reads a `<...>` construct starting at the current cursor.
///
/// Caller is expected to have peeked `<` but NOT consumed it.
///
/// Returns `FragmentChild::Comment` for `<!-- -->`, otherwise a
/// `FragmentChild::RegularElement` (component / special-element variants are
/// deferred — they fall back to `RegularElement` shape for now).
pub fn read_element_or_comment(
    parser: &mut Parser<'_>,
) -> Result<FragmentChild, CompileDiagnostic> {
    debug_assert!(parser.match_str("<"));
    let start = parser.index;

    // Comment short-circuit: `<!--` is handled by `state/comment.rs`.
    if parser.template[parser.index..].starts_with("<!--") {
        let c = super::comment::read_comment(parser)?;
        return Ok(FragmentChild::Comment(c));
    }

    // Consume `<`.
    parser.index += 1;

    if parser.match_str("/") {
        // `</foo>` at fragment top level with no matching open. Read the
        // tag name to surface the upstream error code.
        parser.index += 1; // consume `/`
        let _close_name_start = parser.index;
        let close_name = parser.read_while(|b| {
            b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_' || b == b':' || b == b'!'
        });
        let close_name: String = close_name.into();
        if is_void(&close_name) {
            return Err(svelte_diagnostics::errors::void_element_invalid_content(
                Some((start as u32, start as u32)),
            ));
        }
        // If the most recent auto-close was for this tag, surface
        // `element_invalid_closing_tag_autoclosed` — upstream signals to
        // the user that the parser already closed the element when it
        // hit a non-nestable child (e.g. `<pre>` inside `<p>`). Mirrors
        // upstream's `last_auto_closed_tag` check at element.js:111-126.
        if let Some(last) = parser.last_auto_closed_tag.take() {
            if last.tag == close_name {
                // `closer` is e.g. `<pre>` — strip the angle brackets
                // (and optional `/`) so the error message reads
                // `cannot nest \`<pre>\` inside \`<p>\``.
                let trimmed = last
                    .closer
                    .trim_start_matches('<')
                    .trim_start_matches('/')
                    .trim_end_matches('>');
                return Err(
                    svelte_diagnostics::errors::element_invalid_closing_tag_autoclosed(
                        Some((start as u32, start as u32)),
                        &close_name,
                        trimmed,
                    ),
                );
            }
        }
        return Err(svelte_diagnostics::errors::element_invalid_closing_tag(
            Some((start as u32, parser.index as u32)),
            &close_name,
        ));
    }

    let name = read_tag_name(parser)?;
    parser.element_depth += 1;
    // Drop-guard so every early return decrements element_depth.
    struct DepthGuard<'p, 'src: 'p>(&'p mut Parser<'src>);
    impl Drop for DepthGuard<'_, '_> {
        fn drop(&mut self) {
            self.0.element_depth = self.0.element_depth.saturating_sub(1);
            // Clear last_auto_closed_tag if we've popped past where it
            // was set (mirrors upstream's element.js:133-134).
            if let Some(last) = self.0.last_auto_closed_tag.as_ref() {
                if self.0.element_depth < last.depth {
                    self.0.last_auto_closed_tag = None;
                }
            }
        }
    }
    let _depth_guard = DepthGuard(parser);
    let parser = &mut *_depth_guard.0;

    if is_svelte_meta_name(&name) {
        // TODO(2c follow-up): SvelteBody / SvelteHead / etc.
        // For now, fall through and treat as a RegularElement — the harness
        // diff will surface the divergence on those fixtures.
    }

    let name_loc = SourceLocation {
        start: parser.line_map.position(start + 1), // after `<`
        end: parser.line_map.position(start + 1 + name.len()),
    };

    // `<script>` and `<style>` use a static attribute reader that does NOT
    // treat `{` inside quoted values as a mustache interpolation. This is so
    // that e.g. `<script lang="ts" generics="T extends { foo: number }">`
    // works — the `{...}` is just literal text. Mirrors
    // `is_top_level_script_or_style` in element.js:228-232.
    let static_attrs = name == "script" || name == "style";
    let attributes = read_attributes(parser, static_attrs)?;

    parser.allow_whitespace();

    // Self-closing `/>`.
    let self_closing = parser.eat("/>");

    if !self_closing {
        // Otherwise `>` is required.
        if !parser.eat(">") {
            if parser.index >= parser.template.len() {
                return Err(svelte_diagnostics::errors::unexpected_eof(Some((
                    parser.index as u32,
                    parser.index as u32,
                ))));
            }
            return Err(errors::expected_token(
                Some((parser.index as u32, parser.index as u32)),
                ">",
            ));
        }
    }

    // `element_invalid_self_closing_tag` warning — emit when an HTML tag
    // uses `/>` syntax but isn't a void element AND isn't a Component AND
    // isn't a foreign-namespace tag (svg, math, or a `ns:tag` syntax) AND
    // isn't `<slot>` (Web Components slot). Mirrors `element.js:407-414`.
    if self_closing
        && !is_void(&name)
        && !is_component_name(&name)
        && !name.contains(':')
        && !is_svg_foreign(&name)
        && name != "slot"
    {
        parser
            .warnings
            .push(svelte_diagnostics::warnings::element_invalid_self_closing_tag(
                Some((start as u32, parser.index as u32)),
                &name,
            ));
    }

    // Void elements never have a body.
    if self_closing || is_void(&name) {
        return Ok(build_element(
            name.into_owned(),
            start as u32,
            parser.index as u32,
            name_loc,
            attributes,
            Fragment::empty(),
            parser.shadowroot_depth > 0,
        ));
    }

    // `<template shadowrootmode="...">` marks its subtree as shadow-root
    // territory. Inside, `<slot>` is a RegularElement, not Svelte's
    // SlotElement (`parent_is_shadowroot_template` in element.js:455-468).
    let entered_shadowroot =
        name == "template" && attributes.iter().any(|a| match a {
            ElementAttribute::Attribute(attr) => attr.name == "shadowrootmode",
            _ => false,
        });
    if entered_shadowroot {
        parser.shadowroot_depth += 1;
    }

    // `<script>` and `<style>` bodies are raw-text — their contents are
    // consumed verbatim up to the matching close tag without trying to
    // parse them as Svelte markup. (`{...}` inside CSS / JS is not a
    // mustache; this is what `phases/1-parse/state/element.js` mirrors via
    // `read_script` / `read_style`.)
    //
    // Phase 2f/2g will replace this raw-body with real parsers feeding
    // OXC (for script) and svelte_css_parser (for style). Until then,
    // attaching a single Text child is wrong shape but lets the rest of
    // the document continue to parse cleanly.
    let (fragment, implicit_close) = if name == "script" || name == "style" {
        (read_raw_until_close_tag(parser, &name)?, false)
    } else if name == "textarea" {
        // `<textarea>` contents are a sequence of text and `{expr}` only —
        // no nested elements. Mirrors element.js:401-410.
        (read_textarea_fragment(parser)?, false)
    } else {
        parse_fragment_until_close_tag(parser, &name)?
    };

    if entered_shadowroot {
        parser.shadowroot_depth -= 1;
    }

    // Implicit-close (HTML auto-close rules, e.g. `<li>...<li>`): leave the
    // parent's close tag to the caller. The element ends right at the
    // current parser position. Matches element.js:213-222.
    if implicit_close {
        // `element_implicitly_closed` — warn so users can add an explicit
        // close tag. The closing token is whatever appears at the cursor
        // (e.g. `</main>` or `<p>`). Mirrors element.js:215-220.
        let closer = peek_implicit_closer(parser.template, parser.index);
        if !is_void(&name) && !is_component_name(&name) {
            parser
                .warnings
                .push(svelte_diagnostics::warnings::element_implicitly_closed(
                    Some((start as u32, parser.index as u32)),
                    &name,
                    &closer,
                ));
        }
        // Record the auto-close so a later stray `</name>` can surface
        // `element_invalid_closing_tag_autoclosed` instead of the generic
        // closing-tag error. Mirrors upstream's `parser.last_auto_closed_tag`.
        parser.last_auto_closed_tag = Some(crate::parser::LastAutoClosed {
            tag: name.to_string(),
            closer: closer.clone(),
            // Depth AFTER the implicit pop — mirrors upstream which sets
            // `depth = parser.stack.length` AFTER `parser.pop()` at
            // element.js:131-132. Means: clear once we've popped past
            // the *parent* of the auto-closed element.
            depth: parser.element_depth.saturating_sub(1),
        });
        return Ok(build_element(
            name.into_owned(),
            start as u32,
            parser.index as u32,
            name_loc,
            attributes,
            fragment,
            parser.shadowroot_depth > 0,
        ));
    }

    // `<textarea>` has a relaxed close-tag form: `</textarea(\s[^>]*)?>`. We
    // already located it via `textarea_close_at`; consume the whole regex
    // here rather than going through `read_tag_name` which would reject the
    // junk between `</textarea` and `>`.
    if name == "textarea" {
        debug_assert!(textarea_close_at(parser.template, parser.index));
        let bytes = parser.template.as_bytes();
        parser.index += "</textarea".len(); // past `</textarea`
        // Find the next `>` and advance past it (any chars allowed in
        // between, including whitespace and arbitrary text).
        while parser.index < bytes.len() && bytes[parser.index] != b'>' {
            parser.index += 1;
        }
        if parser.index < bytes.len() {
            parser.index += 1; // past `>`
        }
        return Ok(build_element(
            name.into_owned(),
            start as u32,
            parser.index as u32,
            name_loc,
            attributes,
            fragment,
            parser.shadowroot_depth > 0,
        ));
    }

    // Consume `</name>`.
    if !parser.eat("</") {
        // For `<script>` whose body is empty AND we hit EOF, upstream
        // emits `unexpected_eof` instead of `element_unclosed`.
        if name == "script" && fragment.nodes.iter().all(|n| match n {
            FragmentChild::Text(t) => t.data.is_empty(),
            _ => false,
        }) {
            return Err(svelte_diagnostics::errors::unexpected_eof(Some((
                parser.index as u32,
                parser.index as u32,
            ))));
        }
        // `<style>` empty body EOF: upstream emits `expected_token`
        // ("Expected token </style"). Match that.
        if name == "style" && fragment.nodes.iter().all(|n| match n {
            FragmentChild::Text(t) => t.data.is_empty(),
            _ => false,
        }) {
            return Err(errors::expected_token(
                Some((parser.index as u32, parser.index as u32)),
                "</style",
            ));
        }
        // `<style>` with body but no close tag: upstream invokes the CSS
        // parser on the body inline (style.js:25). If CSS parsing fails,
        // that diagnostic surfaces FIRST, before the unclosed-tag error.
        if name == "style" {
            if let Some(FragmentChild::Text(t)) = fragment.nodes.first() {
                let css_attrs: Vec<svelte_ast::ElementAttribute> = attributes.clone();
                if let Err(css_err) = svelte_css_parser::read_style(
                    parser.template,
                    start as u32,
                    t.start as usize,
                    css_attrs,
                    None,
                ) {
                    return Err(css_err);
                }
            }
        }
        return Err(errors::element_unclosed(
            Some((start as u32, (start + 1) as u32)),
            &name,
        ));
    }
    let close_start = parser.index;
    let close_name = read_tag_name(parser)?;
    if close_name != name {
        return Err(errors::element_invalid_closing_tag(
            Some((close_start as u32, parser.index as u32)),
            &close_name,
        ));
    }
    parser.allow_whitespace();
    if !parser.eat(">") {
        return Err(errors::expected_token(
            Some((parser.index as u32, parser.index as u32)),
            ">",
        ));
    }

    Ok(build_element(
        name.into_owned(),
        start as u32,
        parser.index as u32,
        name_loc,
        attributes,
        fragment,
        parser.shadowroot_depth > 0,
    ))
}

/// Choose the right `FragmentChild` variant based on the tag name. `slot` and
/// `title` are special HTML elements with their own AST variants in
/// `template.d.ts`; everything else (including custom elements) becomes a
/// `RegularElement`.
///
/// `svelte:*` meta tags and uppercase Component invocations fall through to
/// `RegularElement` for now — Phase 2c follow-up will route them to their
/// dedicated AST variants.
fn build_element(
    name: String,
    start: u32,
    end: u32,
    name_loc: SourceLocation,
    attributes: Vec<ElementAttribute>,
    fragment: Fragment,
    inside_shadowroot: bool,
) -> FragmentChild {
    match name.as_str() {
        // `<slot>` inside a `<template shadowrootmode>` ancestor is a real
        // DOM slot element, not Svelte's SlotElement (element.js:174).
        "slot" if !inside_shadowroot => FragmentChild::SlotElement(SlotElement {
            start,
            end,
            name_loc,
            attributes,
            fragment,
        }),
        "title" => FragmentChild::TitleElement(TitleElement {
            start,
            end,
            name_loc,
            attributes,
            fragment,
        }),
        "svelte:body" => FragmentChild::SvelteBody(SvelteBody {
            start,
            end,
            name_loc,
            attributes,
            fragment,
        }),
        "svelte:boundary" => FragmentChild::SvelteBoundary(SvelteBoundary {
            start,
            end,
            name_loc,
            attributes,
            fragment,
        }),
        "svelte:document" => FragmentChild::SvelteDocument(SvelteDocument {
            start,
            end,
            name_loc,
            attributes,
            fragment,
        }),
        "svelte:fragment" => FragmentChild::SvelteFragment(SvelteFragment {
            start,
            end,
            name_loc,
            attributes,
            fragment,
        }),
        "svelte:head" => FragmentChild::SvelteHead(SvelteHead {
            start,
            end,
            name_loc,
            attributes,
            fragment,
        }),
        "svelte:options" => FragmentChild::SvelteOptions(SvelteOptionsRaw {
            start,
            end,
            name_loc,
            attributes,
            fragment,
        }),
        "svelte:self" => FragmentChild::SvelteSelf(SvelteSelf {
            start,
            end,
            name_loc,
            attributes,
            fragment,
        }),
        "svelte:window" => FragmentChild::SvelteWindow(SvelteWindow {
            start,
            end,
            name_loc,
            attributes,
            fragment,
        }),
        // `<svelte:component this={...}>` — extract `this` attribute as
        // `expression`. Mirrors element.js:265-281.
        "svelte:component" => {
            let (mut attrs, this_value) = extract_this_attribute(attributes);
            let expression = build_svelte_this_expression(&this_value, &name);
            let _ = &mut attrs;
            FragmentChild::SvelteComponent(Box::new(svelte_ast::SvelteComponent {
                start,
                end,
                name_loc,
                attributes: attrs,
                fragment,
                expression,
            }))
        }
        // `<svelte:element this={...}>` — extract `this` attribute as
        // `tag`. Mirrors element.js:283-326.
        "svelte:element" => {
            let (attrs, this_value) = extract_this_attribute(attributes);
            let tag = build_svelte_this_expression(&this_value, &name);
            FragmentChild::SvelteElement(Box::new(svelte_ast::SvelteElement {
                start,
                end,
                name_loc,
                attributes: attrs,
                fragment,
                tag,
            }))
        }
        n if is_component_name(n) => FragmentChild::Component(Box::new(svelte_ast::Component {
            start,
            end,
            name,
            name_loc,
            attributes,
            fragment,
            metadata: Default::default(),
        })),
        _ => FragmentChild::RegularElement(RegularElement {
            start,
            end,
            name,
            name_loc,
            attributes,
            fragment,
            metadata: Default::default(),
        }),
    }
}

/// Find the `this` attribute in `attributes` and splice it out, returning
/// its value separately. Mirrors `findIndex + splice` in element.js:266-275.
fn extract_this_attribute(
    mut attributes: Vec<ElementAttribute>,
) -> (Vec<ElementAttribute>, Option<AttributeValue>) {
    let idx = attributes
        .iter()
        .position(|a| matches!(a, ElementAttribute::Attribute(attr) if attr.name == "this"));
    if let Some(i) = idx {
        let attr = attributes.remove(i);
        if let ElementAttribute::Attribute(a) = attr {
            return (attributes, Some(a.value));
        }
    }
    (attributes, None)
}

/// Build the typed `Expression` for `<svelte:component>` / `<svelte:element>`'s
/// `this={...}` attribute. Unwraps `ExpressionTag.expression`; for string
/// values builds a `StringLiteral`. Falls back to a placeholder Identifier
/// when missing — upstream errors via `e.svelte_*_missing_this`, but we
/// keep parsing.
fn build_svelte_this_expression(
    value: &Option<AttributeValue>,
    _tag_name: &str,
) -> svelte_js_ast::Expression {
    fn placeholder() -> svelte_js_ast::Expression {
        svelte_js_ast::Expression::Identifier(svelte_js_ast::Identifier {
            name: Cow::Borrowed("__missing_this__"),
            span: svelte_js_ast::Span::ZERO,
        })
    }
    let Some(v) = value else { return placeholder() };
    match v {
        AttributeValue::Empty => placeholder(),
        AttributeValue::Single(tag) => tag.expression.clone(),
        AttributeValue::Many(parts) => {
            let non_empty: Vec<&AttributeValuePart> = parts
                .iter()
                .filter(|p| match p {
                    AttributeValuePart::Text(t) => !t.raw.is_empty(),
                    _ => true,
                })
                .collect();
            if non_empty.len() == 1 {
                match non_empty[0] {
                    AttributeValuePart::ExpressionTag(e) => e.expression.clone(),
                    AttributeValuePart::Text(t) => {
                        svelte_js_ast::Expression::Literal(Box::new(
                            svelte_js_ast::Literal::String(svelte_js_ast::StringLiteral {
                                value: Cow::Owned(t.data.clone()),
                                raw: Some(format!("'{}'", t.raw)),
                                span: svelte_js_ast::Span::new(t.start, t.end),
                            }),
                        ))
                    }
                }
            } else {
                placeholder()
            }
        }
    }
}

/// A tag name resolves to a Component if it matches upstream's
/// `regex_valid_component_name`:
/// - Starts with an uppercase letter; rest is identifier-continue chars
///   plus `.` (e.g. `MyComponent`, `Lib.Modal`).
/// - OR starts with an ID_Start char (any case), one or more
///   identifier-continue chars, then at least one `.NAME` segment
///   (e.g. `lib.Modal`).
fn is_component_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else { return false };
    let is_id_continue = |c: char| {
        c == '$' || c == '_' || c == '\u{200c}' || c == '\u{200d}'
            || unicode_ident::is_xid_continue(c)
    };
    if first.is_uppercase() {
        // Uppercase form — rest can be id-continue or `.`.
        return chars.all(|c| c == '.' || is_id_continue(c));
    }
    // Dot-notation form: ID_Start (any case) then `id-continue*` then one or
    // more `.id-continue+` segments.
    if unicode_ident::is_xid_start(first) || first == '$' || first == '_' {
        // After the head, walk segments separated by `.`.
        let rest = chars.as_str();
        if !rest.contains('.') {
            return false;
        }
        for (i, seg) in rest.split('.').enumerate() {
            if i == 0 {
                if !seg.chars().all(is_id_continue) {
                    return false;
                }
            } else {
                if seg.is_empty() {
                    return false;
                }
                if !seg.chars().all(is_id_continue) {
                    return false;
                }
            }
        }
        return true;
    }
    false
}

/// Read a tag name. Returns the slice consumed (borrowed from the source).
/// Peek the implicit-closer tag at `index` (e.g. `</main>` or `<p>`) and
/// return it as a printable string for the `element_implicitly_closed`
/// message. Returns an empty string if nothing recognizable is there.
fn peek_implicit_closer(template: &str, index: usize) -> String {
    let rest = &template[index..];
    if rest.starts_with("</") {
        // `</main>` — read tag name then close.
        let bytes = rest.as_bytes();
        let mut i = 2;
        while i < bytes.len()
            && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-')
        {
            i += 1;
        }
        return format!("</{}>", &rest[2..i]);
    }
    if rest.starts_with('<') {
        let bytes = rest.as_bytes();
        let mut i = 1;
        while i < bytes.len()
            && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-')
        {
            i += 1;
        }
        return format!("<{}>", &rest[1..i]);
    }
    String::new()
}

/// True if `name` is inside a foreign-namespace where self-closing is
/// legitimate. SVG and MathML tags can self-close.
fn is_svg_foreign(name: &str) -> bool {
    matches!(
        name,
        "circle" | "ellipse" | "line" | "rect" | "path" | "polygon" | "polyline"
        | "g" | "svg" | "defs" | "use" | "symbol" | "linearGradient"
        | "radialGradient" | "stop" | "image" | "text" | "tspan" | "marker"
        | "mask" | "pattern" | "clipPath" | "filter" | "foreignObject"
        | "mpath" | "set" | "animate" | "animateMotion" | "animateTransform"
        | "math" | "mspace" | "mi" | "mn" | "mo" | "mrow" | "mfrac" | "msup"
        | "msub" | "msubsup" | "mfenced" | "mroot" | "msqrt" | "mtext"
    )
}

fn read_tag_name<'src>(parser: &mut Parser<'src>) -> Result<std::borrow::Cow<'src, str>, CompileDiagnostic> {
    let start = parser.index;
    if parser.index >= parser.template.len() {
        return Err(svelte_diagnostics::errors::unexpected_eof(Some((
            parser.index as u32,
            parser.index as u32,
        ))));
    }
    // Mirrors upstream's `read_until(/(\s|\/|>)/)` — consume everything up
    // to whitespace, `/`, or `>`. Validation happens after, so syntactically
    // invalid names get a `tag_invalid_name` diagnostic instead of being
    // truncated silently.
    let name = parser.read_while(|b| {
        !matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'/' | b'>')
    });
    if !is_valid_tag_name(name)
        && !is_svelte_meta_name(name)
        && !is_component_name(name)
        && !name.eq_ignore_ascii_case("!doctype")
    {
        return Err(svelte_diagnostics::errors::tag_invalid_name(Some((
            start as u32,
            parser.index as u32,
        ))));
    }
    Ok(std::borrow::Cow::Borrowed(name))
}

/// Read all attributes up to (but not including) `>` or `/>`. JS-style
/// `//` and `/* */` comments between attributes are stripped and pushed
/// into `parser.comments` (mirrors `read_attribute`'s comment loop at
/// `element.js:520-528`).
///
/// `static_only` is set for `<script>` / `<style>` opening tags — those use
/// `read_static_attribute` upstream, which doesn't honour `{...}`
/// interpolations inside quoted values.
fn read_attributes(
    parser: &mut Parser<'_>,
    static_only: bool,
) -> Result<Vec<ElementAttribute>, CompileDiagnostic> {
    let mut out = Vec::with_capacity(4);
    loop {
        parser.allow_whitespace();
        // Drain any number of `//` / `/* */` comments that sit between
        // attributes (or after the tag name and before the first attribute).
        while read_attr_comment(parser) {
            parser.allow_whitespace();
        }
        if parser.match_str(">") || parser.match_str("/>") || parser.peek().is_none() {
            break;
        }
        if parser.peek() == Some(b'{') {
            // `{@attach expr}` — element-level attach directive.
            if peek_at_tag_keyword(parser, "attach") {
                out.push(ElementAttribute::AttachTag(read_attach_tag(parser)?));
                continue;
            }
            // `{...spread}` and `{name}` shorthand attribute. Mirrors the
            // `{`-prefixed branch of `read_attribute` in element.js:530-606.
            // In static-only mode (`<script>` / `<style>`), parse it as a
            // soft `script_unknown_attribute` warning rather than erroring,
            // mirroring upstream's lenient behavior in element.js:583-589.
            let brace_start = parser.index;
            let parsed = read_braced_attribute(parser)?;
            if static_only {
                parser
                    .warnings
                    .push(svelte_diagnostics::warnings::script_unknown_attribute(
                        Some((brace_start as u32, parser.index as u32)),
                    ));
            } else {
                out.push(parsed);
            }
            continue;
        }
        out.push(read_attribute(parser, static_only)?);
    }
    // Duplicate-attribute check: same name appearing twice on the same
    // element. `bind:foo` and `foo` both produce attribute `foo` for this
    // purpose. Mirrors upstream's attribute_duplicate.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for a in &out {
        let (name, span) = match a {
            ElementAttribute::Attribute(att) => (att.name.clone(), (att.start, att.end)),
            ElementAttribute::BindDirective(b) => (b.name.clone(), (b.start, b.end)),
            _ => continue,
        };
        if !seen.insert(name) {
            return Err(svelte_diagnostics::errors::attribute_duplicate(Some(span)));
        }
    }
    Ok(out)
}

/// Parse a `{...spread}` or `{name}` shorthand attribute. Caller has
/// confirmed the cursor is on `{`.
fn read_braced_attribute(
    parser: &mut Parser<'_>,
) -> Result<ElementAttribute, CompileDiagnostic> {
    debug_assert_eq!(parser.peek(), Some(b'{'));
    let start = parser.index;
    parser.index += 1; // `{`
    parser.allow_whitespace();

    // `{...expr}` — SpreadAttribute.
    if parser.template[parser.index..].starts_with("...") {
        parser.index += 3;
        let (expression, expr_end) = parser.parse_expression_at(parser.index)?;
        parser.index = expr_end;
        parser.allow_whitespace();
        if !parser.eat("}") {
            return Err(errors::expected_token(
                Some((parser.index as u32, parser.index as u32)),
                "}",
            ));
        }
        return Ok(ElementAttribute::SpreadAttribute(svelte_ast::SpreadAttribute {
            start: start as u32,
            end: parser.index as u32,
            expression,
        }));
    }

    // `{name}` — shorthand attribute. Upstream uses `parser.read_identifier()`
    // (which uses `state.locator`), so the Identifier's `loc` includes
    // `character`. We mirror that by reading the identifier bytewise here and
    // constructing the Identifier directly via `LineMap.position()`.
    let id_start = parser.index;
    let id_bytes = parser.read_while(|b| {
        b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
    });
    if id_bytes.is_empty() {
        // `{}` — empty braced attribute. Upstream's specific error code.
        if parser.peek() == Some(b'}') {
            return Err(svelte_diagnostics::errors::attribute_empty_shorthand(
                Some((start as u32, parser.index as u32)),
            ));
        }
        return Err(errors::expected_token(
            Some((id_start as u32, id_start as u32)),
            "identifier",
        ));
    }
    let id_name: String = id_bytes.into();
    let id_end = parser.index;
    parser.allow_whitespace();
    if !parser.eat("}") {
        return Err(errors::expected_token(
            Some((parser.index as u32, parser.index as u32)),
            "}",
        ));
    }

    let id_loc_start = parser.line_map.position(id_start);
    let id_loc_end = parser.line_map.position(id_end);
    let id_expr = svelte_js_ast::Expression::Identifier(svelte_js_ast::Identifier {
        name: Cow::Owned(id_name.clone()),
        span: svelte_js_ast::Span::new(id_start as u32, id_end as u32),
    });

    let expression_tag = ExpressionTag {
        start: id_start as u32,
        end: id_end as u32,
        expression: id_expr,
    };

    let name_loc = SourceLocation {
        start: id_loc_start,
        end: id_loc_end,
    };

    Ok(ElementAttribute::Attribute(Attribute {
        start: start as u32,
        end: parser.index as u32,
        name: id_name,
        name_loc: Some(name_loc),
        value: AttributeValue::Single(expression_tag),
    }))
}

fn position_to_json(p: &svelte_ast::Position) -> serde_json::Value {
    let mut o = serde_json::Map::new();
    o.insert("line".to_string(), serde_json::json!(p.line));
    o.insert("column".to_string(), serde_json::json!(p.column));
    if let Some(c) = p.character {
        o.insert("character".to_string(), serde_json::json!(c));
    }
    serde_json::Value::Object(o)
}

/// Try to read a JS-style comment (`//...` or `/* ... */`) at the current
/// position. Returns true if a comment was consumed (and pushed onto
/// `parser.comments`). Mirrors `read_comment` in `element.js:730-768`.
fn read_attr_comment(parser: &mut Parser<'_>) -> bool {
    let bytes = parser.template.as_bytes();
    let i = parser.index;
    if i + 2 > bytes.len() {
        return false;
    }
    if bytes[i] == b'/' && bytes[i + 1] == b'/' {
        let start = i;
        let mut j = i + 2;
        while j < bytes.len() && bytes[j] != b'\n' {
            j += 1;
        }
        let value = parser.template[start + 2..j].into();
        parser.index = j;
        parser.comments.push(crate::oxc_bridge::RawComment {
            line: true,
            start: start as u32,
            end: j as u32,
            value,
            with_character: true,
        });
        return true;
    }
    if bytes[i] == b'/' && bytes[i + 1] == b'*' {
        let start = i;
        let mut j = i + 2;
        while j + 1 < bytes.len() && !(bytes[j] == b'*' && bytes[j + 1] == b'/') {
            j += 1;
        }
        let value_end = j;
        // Consume `*/` if present.
        if j + 1 < bytes.len() && bytes[j] == b'*' && bytes[j + 1] == b'/' {
            j += 2;
        }
        let value = parser.template[start + 2..value_end].into();
        parser.index = j;
        parser.comments.push(crate::oxc_bridge::RawComment {
            line: false,
            start: start as u32,
            end: j as u32,
            value,
            with_character: true,
        });
        return true;
    }
    false
}

/// Peek: does the cursor sit on `{` followed by (ws) `@<keyword>`?
fn peek_at_tag_keyword(parser: &Parser<'_>, keyword: &str) -> bool {
    let after = match parser.template[parser.index..].strip_prefix('{') {
        Some(s) => s.trim_start(),
        None => return false,
    };
    let after = match after.strip_prefix('@') {
        Some(s) => s,
        None => return false,
    };
    if !after.starts_with(keyword) {
        return false;
    }
    // After the keyword we must see whitespace or `(` or `}` — not another
    // identifier char (so `attaches` doesn't match `attach`).
    matches!(
        after.as_bytes().get(keyword.len()),
        None | Some(b' ' | b'\t' | b'\n' | b'\r' | b'(' | b'}')
    )
}

/// Parse `{@attach <expression>}` in an element attribute list. Caller has
/// confirmed the cursor is at `{`.
fn read_attach_tag(parser: &mut Parser<'_>) -> Result<AttachTag, CompileDiagnostic> {
    debug_assert_eq!(parser.peek(), Some(b'{'));
    let start = parser.index;
    parser.index += 1; // `{`
    parser.allow_whitespace();
    debug_assert_eq!(parser.peek(), Some(b'@'));
    parser.index += 1; // `@`
    // `attach`
    debug_assert!(parser.match_str("attach"));
    parser.index += "attach".len();
    parser.allow_whitespace();
    let (expression, expr_end) = parser.parse_expression_at(parser.index)?;
    parser.index = expr_end;
    parser.allow_whitespace();
    if !parser.eat("}") {
        return Err(errors::expected_token(
            Some((parser.index as u32, parser.index as u32)),
            "}",
        ));
    }
    Ok(AttachTag {
        start: start as u32,
        end: parser.index as u32,
        expression,
    })
}

/// Parse `{expression}` inside an attribute value. Caller has confirmed
/// the cursor is at `{`. Returns the constructed `ExpressionTag`.
fn read_expression_tag(parser: &mut Parser<'_>) -> Result<ExpressionTag, CompileDiagnostic> {
    debug_assert_eq!(parser.peek(), Some(b'{'));
    let start = parser.index;
    parser.index += 1; // `{`
    parser.allow_whitespace();
    // `{ />` or `{ >` inside an attribute value — the brace was never
    // matched. Upstream surfaces this as `expected_token "}"` at the
    // position right after the brace; we preempt OXC's `js_parse_error`
    // here so the diagnostic code matches. Be careful not to match
    // `/* ... */` JS comments (which legitimately start with `/`).
    let next_two = parser
        .template
        .as_bytes()
        .get(parser.index..parser.index + 2)
        .unwrap_or(&[]);
    if next_two == b"/>" || matches!(parser.peek(), Some(b'>')) {
        return Err(errors::expected_token(
            Some((parser.index as u32, parser.index as u32)),
            "}",
        ));
    }
    let (expression, expr_end) = parser.parse_expression_at(parser.index)?;
    parser.index = expr_end;
    parser.allow_whitespace();
    if !parser.eat("}") {
        return Err(errors::expected_token(
            Some((parser.index as u32, parser.index as u32)),
            "}",
        ));
    }
    Ok(ExpressionTag {
        start: start as u32,
        end: parser.index as u32,
        expression,
    })
}

/// Maps a directive prefix (before the colon) to its AST node kind.
/// Mirrors `get_directive_type` in `element.js:774-784`.
enum DirectiveKind {
    Use,
    Animate,
    Bind,
    Class,
    Style,
    On,
    Let,
    /// `in:`, `out:`, `transition:` — the prefix determines intro/outro.
    Transition,
}

fn directive_kind(prefix: &str) -> Option<DirectiveKind> {
    match prefix {
        "use" => Some(DirectiveKind::Use),
        "animate" => Some(DirectiveKind::Animate),
        "bind" => Some(DirectiveKind::Bind),
        "class" => Some(DirectiveKind::Class),
        "style" => Some(DirectiveKind::Style),
        "on" => Some(DirectiveKind::On),
        "let" => Some(DirectiveKind::Let),
        "in" | "out" | "transition" => Some(DirectiveKind::Transition),
        _ => None,
    }
}

fn read_attribute(
    parser: &mut Parser<'_>,
    static_only: bool,
) -> Result<ElementAttribute, CompileDiagnostic> {
    let start = parser.index;

    // Attribute name: any byte that isn't whitespace, `=`, `/`, `>`, `"`, `'`.
    // (Mirrors `regex_token_ending_character` in element.js:24.)
    let name_start = parser.index;
    let name = parser.read_while(|b| {
        !matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'=' | b'/' | b'>' | b'"' | b'\'')
    });
    if name.is_empty() {
        return Err(errors::expected_token(
            Some((start as u32, parser.index as u32)),
            "attribute name",
        ));
    }
    let name_end = parser.index;
    let raw_name: String = name.into();

    // Per upstream `read_tag` (element.js:935-950), name_loc spans the entire
    // raw name (including any directive prefix like `bind:`).
    let name_loc = SourceLocation {
        start: parser.line_map.position(name_start),
        end: parser.line_map.position(name_end),
    };

    let value = if parser.eat("=") {
        parser.allow_whitespace();
        // Special case: `=/>` → value is the single `/`. Mirrors
        // element.js:626-638. (Without this, `<a href=/>` would lose the `/`
        // because our `/>` boundary would stop the sequence at byte 0.)
        if parser.peek() == Some(b'/')
            && parser.template[parser.index + 1..].starts_with('>')
        {
            let char_start = parser.index;
            parser.index += 1;
            AttributeValue::Many(vec![AttributeValuePart::Text(Text {
                start: char_start as u32,
                end: (char_start + 1) as u32,
                raw: "/".to_string(),
                data: "/".to_string(),
            })])
        } else if static_only {
            read_static_attribute_value(parser)?
        } else {
            read_attribute_value(parser)?
        }
    } else {
        AttributeValue::Empty
    };
    let end = parser.index;

    // Directive detection: split on the first `:` and look up the prefix.
    // Static-only mode (script/style) returns a plain Attribute regardless.
    if !static_only {
    if let Some(colon_index) = raw_name.find(':') {
        if let Some(kind) = directive_kind(&raw_name[..colon_index]) {
            // After the colon: `name|modifier1|modifier2`.
            let rest = &raw_name[colon_index + 1..];
            let mut parts = rest.split('|');
            let directive_name: String = parts.next().unwrap_or("").into();
            let modifiers: Vec<String> = parts.map(|s| s.into()).collect();

            if directive_name.is_empty() {
                return Err(errors::directive_missing_name(
                    Some((start as u32, (start + colon_index + 1) as u32)),
                    &raw_name,
                ));
            }

            return Ok(build_directive(
                kind,
                &raw_name,
                colon_index,
                directive_name,
                modifiers,
                name_loc,
                value,
                start,
                end,
            )?);
        }
    }
    } // close `if !static_only`

    Ok(ElementAttribute::Attribute(Attribute {
        start: start as u32,
        end: end as u32,
        name: raw_name,
        name_loc: Some(name_loc),
        value,
    }))
}

/// Read an attribute value treating any `{`/`}` inside as literal text.
/// Used for `<script>` / `<style>` attributes. Mirrors
/// `read_static_attribute` in element.js:475-514.
fn read_static_attribute_value(
    parser: &mut Parser<'_>,
) -> Result<AttributeValue, CompileDiagnostic> {
    // Quoted form: read until matching quote, no interpolations.
    let opener = parser.peek();
    let quote: Option<u8> = match opener {
        Some(b'"') => Some(b'"'),
        Some(b'\'') => Some(b'\''),
        _ => None,
    };

    if let Some(q) = quote {
        parser.index += 1; // opening quote
        let text_start = parser.index;
        while parser.index < parser.template.len() && parser.template.as_bytes()[parser.index] != q {
            parser.index += 1;
        }
        let text_end = parser.index;
        if parser.peek() != Some(q) {
            return Err(errors::expected_token(
                Some((parser.index as u32, parser.index as u32)),
                if q == b'"' { "\"" } else { "'" },
            ));
        }
        parser.index += 1; // closing quote
        let raw: String = parser.template[text_start..text_end].into();
        return Ok(AttributeValue::Many(vec![AttributeValuePart::Text(Text {
            start: text_start as u32,
            end: text_end as u32,
            raw: raw.clone(),
            data: raw,
        })]));
    }

    // Unquoted: read until `>` or whitespace. No interpolations.
    let start = parser.index;
    let raw = parser.read_while(|b| {
        !matches!(
            b,
            b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'"' | b'\'' | b'='
        )
    });
    let end = parser.index;
    if raw.is_empty() {
        return Err(errors::expected_token(
            Some((start as u32, end as u32)),
            "attribute value",
        ));
    }
    Ok(AttributeValue::Many(vec![AttributeValuePart::Text(Text {
        start: start as u32,
        end: end as u32,
        raw: raw.into(),
        data: raw.into(),
    })]))
}

#[allow(clippy::too_many_arguments)]
fn build_directive(
    kind: DirectiveKind,
    raw_name: &str,
    colon_index: usize,
    directive_name: String,
    modifiers: Vec<String>,
    name_loc: SourceLocation,
    value: AttributeValue,
    start: usize,
    end: usize,
) -> Result<ElementAttribute, CompileDiagnostic> {
    // StyleDirective keeps the `value: AttributeValue` shape verbatim
    // (element.js:654-666).
    if matches!(kind, DirectiveKind::Style) {
        return Ok(ElementAttribute::StyleDirective(StyleDirective {
            start: start as u32,
            end: end as u32,
            name: directive_name,
            name_loc: Some(name_loc),
            value,
            modifiers,
        }));
    }

    // Non-style directives extract a single expression from the value.
    // (element.js:669-684.) Bare attribute → expression: None. Single
    // mustache → expression: Some(expr). Quoted value containing exactly one
    // ExpressionTag and nothing else → unwrap that expression (legacy form,
    // `on:click="{handler}"`). Anything else (text content, multiple chunks)
    // → directive_invalid_value error.
    let expression: Option<svelte_js_ast::Expression> = match value {
        AttributeValue::Empty => None,
        AttributeValue::Single(tag) => Some(tag.expression),
        AttributeValue::Many(parts) => {
            // Drop empty leading/trailing Text nodes (zero-length sentinels
            // produced for empty-quoted-content "" but never for actual text).
            let non_empty: Vec<&AttributeValuePart> = parts
                .iter()
                .filter(|p| match p {
                    AttributeValuePart::Text(t) => !t.raw.is_empty(),
                    _ => true,
                })
                .collect();
            if non_empty.len() == 1 {
                if let AttributeValuePart::ExpressionTag(e) = non_empty[0] {
                    Some(e.expression.clone())
                } else {
                    let s = non_empty[0].start_pos();
                    return Err(errors::directive_invalid_value(Some((s, s))));
                }
            } else {
                let first_start = parts
                    .first()
                    .map(|p| p.start_pos())
                    .unwrap_or(start as u32);
                return Err(errors::directive_invalid_value(Some((
                    first_start,
                    first_start,
                ))));
            }
        }
    };

    let directive = match kind {
        DirectiveKind::Use => ElementAttribute::UseDirective(UseDirective {
            start: start as u32,
            end: end as u32,
            name: directive_name,
            name_loc: Some(name_loc),
            expression,
            modifiers,
        }),
        DirectiveKind::Animate => ElementAttribute::AnimateDirective(AnimateDirective {
            start: start as u32,
            end: end as u32,
            name: directive_name,
            name_loc: Some(name_loc),
            expression,
            modifiers,
        }),
        DirectiveKind::Bind => {
            // Bind without expression synthesizes an Identifier from the name
            // (element.js:707-718). The synthesized Identifier carries `start`
            // and `end` but no `loc`.
            let expr = expression.unwrap_or_else(|| {
                svelte_js_ast::Expression::Identifier(svelte_js_ast::Identifier {
                    name: Cow::Owned(directive_name.clone()),
                    span: svelte_js_ast::Span::new(
                        (start + colon_index + 1) as u32,
                        end as u32,
                    ),
                })
            });
            ElementAttribute::BindDirective(BindDirective {
                start: start as u32,
                end: end as u32,
                name: directive_name,
                name_loc: Some(name_loc),
                expression: expr,
                modifiers,
            })
        }
        DirectiveKind::Class => {
            let expr = expression.unwrap_or_else(|| {
                svelte_js_ast::Expression::Identifier(svelte_js_ast::Identifier {
                    name: Cow::Owned(directive_name.clone()),
                    span: svelte_js_ast::Span::new(
                        (start + colon_index + 1) as u32,
                        end as u32,
                    ),
                })
            });
            ElementAttribute::ClassDirective(ClassDirective {
                start: start as u32,
                end: end as u32,
                name: directive_name,
                name_loc: Some(name_loc),
                expression: expr,
                modifiers,
            })
        }
        DirectiveKind::On => ElementAttribute::OnDirective(OnDirective {
            start: start as u32,
            end: end as u32,
            name: directive_name,
            name_loc: Some(name_loc),
            expression,
            modifiers,
        }),
        DirectiveKind::Let => ElementAttribute::LetDirective(LetDirective {
            start: start as u32,
            end: end as u32,
            name: directive_name,
            name_loc: Some(name_loc),
            expression,
            modifiers,
        }),
        DirectiveKind::Transition => {
            let prefix = &raw_name[..colon_index];
            let intro = prefix == "in" || prefix == "transition";
            let outro = prefix == "out" || prefix == "transition";
            ElementAttribute::TransitionDirective(TransitionDirective {
                start: start as u32,
                end: end as u32,
                name: directive_name,
                name_loc: Some(name_loc),
                expression,
                modifiers,
                intro,
                outro,
            })
        }
        DirectiveKind::Style => unreachable!("handled above"),
    };
    Ok(directive)
}

/// Read an attribute value. Mirrors `read_attribute_value` in
/// `element.js:790-841`.
///
/// Three shapes:
/// - `name={expr}` → `AttributeValue::Single(ExpressionTag)` (single mustache).
/// - `name="..."`  → `AttributeValue::Many(Vec<Text|ExpressionTag>)` (quoted
///   sequence; may contain interpolations).
/// - `name=foo`    → `AttributeValue::Many(Vec<Text|ExpressionTag>)` (unquoted
///   sequence; stops at whitespace / `=` / `"` / `'` / `<` / `>` / `` ` `` /
///   `/>`).
///
/// For the unquoted form, mustache interpolations (`name=a{b}c`) are allowed.
fn read_attribute_value(parser: &mut Parser<'_>) -> Result<AttributeValue, CompileDiagnostic> {
    // Mustache-only value: `name={expr}` → `AttributeValue::Single(ExpressionTag)`.
    if parser.peek() == Some(b'{') {
        let tag = read_expression_tag(parser)?;
        // If immediately followed by a trailing `}` (no space), the
        // attribute value is malformed — upstream emits
        // `attribute_unquoted_sequence` at the next analyse pass; we
        // surface it as a parse error to match.
        if parser.peek() == Some(b'}') {
            return Err(svelte_diagnostics::errors::attribute_unquoted_sequence(
                Some((tag.start, parser.index as u32)),
            ));
        }
        return Ok(AttributeValue::Single(tag));
    }

    let opener = parser.peek();
    let quote: Option<u8> = match opener {
        Some(b'"') => Some(b'"'),
        Some(b'\'') => Some(b'\''),
        _ => None,
    };
    // `name=` with no value (next is `>`, whitespace, or `/>`) → upstream
    // emits `expected_attribute_value`.
    if quote.is_none()
        && matches!(
            opener,
            Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r') | Some(b'>') | None
        )
    {
        return Err(svelte_diagnostics::errors::expected_attribute_value(
            Some((parser.index as u32, parser.index as u32)),
        ));
    }

    // Empty quoted value: `name=""` produces a single empty Text node at the
    // position of the second quote. Matches element.js:792-801.
    if let Some(q) = quote {
        parser.index += 1; // opening quote
        if parser.peek() == Some(q) {
            let pos = parser.index;
            parser.index += 1;
            return Ok(AttributeValue::Many(vec![AttributeValuePart::Text(Text {
                start: pos as u32,
                end: pos as u32,
                raw: String::new(),
                data: String::new(),
            })]));
        }
    }

    let parts = read_attr_sequence(parser, quote)?;
    if let Some(q) = quote {
        if parser.peek() != Some(q) {
            return Err(errors::expected_token(
                Some((parser.index as u32, parser.index as u32)),
                if q == b'"' { "\"" } else { "'" },
            ));
        }
        parser.index += 1; // closing quote
    } else if parts.is_empty() {
        return Err(errors::expected_token(
            Some((parser.index as u32, parser.index as u32)),
            "attribute value",
        ));
    }

    // Per element.js:836-840: if unquoted AND single ExpressionTag AND nothing
    // else, hoist to `AttributeValue::Single`. Otherwise return as Many.
    let mut parts = parts;
    if quote.is_none()
        && parts.len() == 1
        && matches!(parts[0], AttributeValuePart::ExpressionTag(_))
    {
        if let AttributeValuePart::ExpressionTag(tag) = parts.remove(0) {
            return Ok(AttributeValue::Single(tag));
        }
    }
    Ok(AttributeValue::Many(parts))
}

/// Read a sequence of Text/ExpressionTag chunks until a stop condition.
/// Mirrors `read_sequence` in element.js:849+ — its callback `done()`
/// determines termination based on context (quote char, or
/// `regex_invalid_unquoted_attribute_value`).
fn read_attr_sequence(
    parser: &mut Parser<'_>,
    quote: Option<u8>,
) -> Result<Vec<AttributeValuePart>, CompileDiagnostic> {
    let mut parts: Vec<AttributeValuePart> = Vec::with_capacity(2);
    let mut text_start = parser.index;

    let flush_text = |parts: &mut Vec<AttributeValuePart>,
                      start: usize,
                      end: usize,
                      tpl: &str| {
        if end > start {
            let raw = &tpl[start..end];
            parts.push(AttributeValuePart::Text(Text {
                start: start as u32,
                end: end as u32,
                raw: raw.into(),
                data: crate::utils::entities::decode_character_references(raw, true),
            }));
        }
    };

    while parser.index < parser.template.len() {
        // Terminator check.
        if let Some(q) = quote {
            if parser.peek() == Some(q) {
                break;
            }
        } else {
            // Unquoted stop chars: `/>` or any of whitespace, `"`, `'`, `=`,
            // `<`, `>`, `` ` ``. Matches `regex_invalid_unquoted_attribute_value`
            // in element.js:20.
            let b = parser.peek().unwrap();
            if matches!(
                b,
                b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'' | b'=' | b'<' | b'>' | b'`'
            ) {
                break;
            }
            if b == b'/' && parser.template[parser.index + 1..].starts_with('>') {
                break;
            }
        }

        if parser.peek() == Some(b'{') {
            // Reject `{#...}` blocks and `{@...}` tags inside attribute
            // values — they're not allowed by upstream. Mirrors
            // `read_sequence` in element.js:878-889.
            let bytes = parser.template.as_bytes();
            let next = bytes.get(parser.index + 1).copied();
            if matches!(next, Some(b'#') | Some(b'@')) {
                let kind = next.unwrap();
                let block_start = parser.index;
                let mut j = parser.index + 2;
                while j < bytes.len() && (bytes[j] as char).is_ascii_lowercase() {
                    j += 1;
                }
                let name = &parser.template[parser.index + 2..j];
                let span = Some((block_start as u32, block_start as u32));
                return Err(if kind == b'#' {
                    svelte_diagnostics::errors::block_invalid_placement(
                        span,
                        name,
                        "attribute value",
                    )
                } else {
                    svelte_diagnostics::errors::tag_invalid_placement(
                        span,
                        name,
                        "attribute value",
                    )
                });
            }
            // Flush any pending text, then parse the mustache.
            flush_text(&mut parts, text_start, parser.index, parser.template);
            let tag = read_expression_tag(parser)?;
            parts.push(AttributeValuePart::ExpressionTag(tag));
            text_start = parser.index;
            continue;
        }

        parser.index += 1;
    }

    flush_text(&mut parts, text_start, parser.index, parser.template);
    Ok(parts)
}

/// Read a `<textarea>` body. The contents are a sequence of Text and
/// ExpressionTag chunks — `<` characters become part of text (no nested
/// elements), but `{expr}` mustaches are still parsed. Stops at the next
/// `</textarea>` (case-insensitive on the closing tag, like upstream's
/// regex_closing_textarea_tag). Caller is responsible for consuming the
/// closing tag.
fn read_textarea_fragment(
    parser: &mut Parser<'_>,
) -> Result<Fragment, CompileDiagnostic> {
    let mut nodes: Vec<FragmentChild> = Vec::with_capacity(8);
    let mut text_start = parser.index;

    let flush_text = |nodes: &mut Vec<FragmentChild>, start: usize, end: usize, tpl: &str| {
        if end > start {
            let raw = &tpl[start..end];
            nodes.push(FragmentChild::Text(Text {
                start: start as u32,
                end: end as u32,
                raw: raw.into(),
                data: crate::utils::entities::decode_character_references(raw, false),
            }));
        }
    };

    while parser.index < parser.template.len() {
        // Stop at the FULL closing tag `</textarea(\s[^>]*)?>` (case-
        // insensitive). Mirrors regex_closing_textarea_tag in element.js:21.
        if textarea_close_at(parser.template, parser.index) {
            break;
        }

        if parser.peek() == Some(b'{') {
            // Reject `{#...}` and `{@...}` inside `<textarea>` — same as
            // attribute values, just a different `location` label.
            let bytes = parser.template.as_bytes();
            let next = bytes.get(parser.index + 1).copied();
            if matches!(next, Some(b'#') | Some(b'@')) {
                let kind = next.unwrap();
                let block_start = parser.index;
                let mut j = parser.index + 2;
                while j < bytes.len() && (bytes[j] as char).is_ascii_lowercase() {
                    j += 1;
                }
                let name = &parser.template[parser.index + 2..j];
                let span = Some((block_start as u32, block_start as u32));
                return Err(if kind == b'#' {
                    svelte_diagnostics::errors::block_invalid_placement(
                        span,
                        name,
                        "<textarea>",
                    )
                } else {
                    svelte_diagnostics::errors::tag_invalid_placement(
                        span,
                        name,
                        "<textarea>",
                    )
                });
            }
            flush_text(&mut nodes, text_start, parser.index, parser.template);
            let tag = read_expression_tag(parser)?;
            nodes.push(FragmentChild::ExpressionTag(tag));
            text_start = parser.index;
            continue;
        }

        let ch = parser.template[parser.index..].chars().next().unwrap();
        parser.index += ch.len_utf8();
    }

    flush_text(&mut nodes, text_start, parser.index, parser.template);
    Ok(Fragment {
        nodes,
        metadata: Default::default(),
    })
}

/// Check whether `template[i..]` matches `</textarea(\s[^>]*)?>` (case-
/// insensitive). Used by the textarea-body reader. Also consumes nothing —
/// the caller is the one who advances past the close tag (via the regular
/// element-closing path in `read_element_or_comment`).
fn textarea_close_at(template: &str, i: usize) -> bool {
    let bytes = template.as_bytes();
    if !template[i..].starts_with("</") {
        return false;
    }
    let after_slash = i + 2;
    let after_name = after_slash + "textarea".len();
    if after_name > bytes.len() {
        return false;
    }
    if !template[after_slash..after_name].eq_ignore_ascii_case("textarea") {
        return false;
    }
    let mut j = after_name;
    // Either immediate `>` or whitespace followed by anything until `>`.
    match bytes.get(j) {
        Some(b'>') => true,
        Some(c) if c.is_ascii_whitespace() => {
            j += 1;
            while j < bytes.len() && bytes[j] != b'>' {
                j += 1;
            }
            j < bytes.len()
        }
        _ => false,
    }
}

/// Consume raw text up to (but not including) `</tag_name>`. Used for the
/// bodies of `<script>` and `<style>`, which are *not* parsed as fragments.
/// Returns an empty `Fragment` (the body is intentionally not exposed as a
/// child of the element — Phase 2f/2g will set `Root.instance` / `Root.css`
/// instead from this raw text).
fn read_raw_until_close_tag(
    parser: &mut Parser<'_>,
    tag_name: &str,
) -> Result<Fragment, CompileDiagnostic> {
    let start = parser.index;
    let close_marker = format!("</{tag_name}");
    while parser.index < parser.template.len() {
        if parser.template[parser.index..].starts_with(&close_marker) {
            let after = &parser.template[parser.index + close_marker.len()..];
            if after.is_empty()
                || matches!(
                    after.as_bytes()[0],
                    b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'/'
                )
            {
                break;
            }
        }
        let ch = parser.template[parser.index..].chars().next().unwrap();
        parser.index += ch.len_utf8();
    }
    // Mirrors element.js:413-441: even an empty body becomes a single
    // Text node with empty `raw`/`data`. Required for fixtures like
    // `<svelte:head><style></style></svelte:head>`.
    let end = parser.index;
    let raw: String = parser.template[start..end].into();
    Ok(Fragment {
        nodes: vec![FragmentChild::Text(Text {
            start: start as u32,
            end: end as u32,
            raw: raw.clone(),
            data: raw,
        })],
        metadata: Default::default(),
    })
}

/// Recursively parse a fragment until the parser cursor sits at `</tag_name>`.
/// Peek the opening-tag name at `template[i..]`. Returns `None` if not an
/// opening tag (closing tag, comment, EOF, doctype). Caller should already
/// have confirmed `<` is at `i`.
fn peek_opening_tag_name(template: &str, i: usize) -> Option<&str> {
    let bytes = template.as_bytes();
    if i >= bytes.len() || bytes[i] != b'<' {
        return None;
    }
    let j = i + 1;
    if j >= bytes.len() {
        return None;
    }
    let first = bytes[j];
    if !first.is_ascii_alphabetic() {
        return None;
    }
    let mut end = j;
    while end < bytes.len()
        && (bytes[end].is_ascii_alphanumeric()
            || bytes[end] == b'-'
            || bytes[end] == b'.'
            || bytes[end] == b'_'
            || bytes[end] == b':')
    {
        end += 1;
    }
    Some(&template[j..end])
}

/// Result of parsing a fragment until close tag. The second component is
/// `true` if the loop terminated because of an HTML implicit-close
/// (e.g. `<li>...<li>` — caller should NOT consume a close tag).
fn parse_fragment_until_close_tag(
    parser: &mut Parser<'_>,
    tag_name: &str,
) -> Result<(Fragment, bool), CompileDiagnostic> {
    let mut nodes: Vec<FragmentChild> = Vec::with_capacity(8);
    let mut implicit_close = false;
    loop {
        if parser.index >= parser.template.len() {
            // EOF before close tag.
            return Err(errors::element_unclosed(
                Some((parser.index as u32, parser.index as u32)),
                tag_name,
            ));
        }

        // Close-tag for *this* element terminates the fragment.
        if parser.match_str("</")
            && parser.template[parser.index + 2..]
                .strip_prefix(tag_name)
                .is_some_and(|rest| {
                    let bytes = rest.as_bytes();
                    bytes.is_empty()
                        || matches!(
                            bytes[0],
                            b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'/'
                        )
                })
        {
            break;
        }

        // Ancestor close-tag terminates this fragment too (implicit close
        // when descendants' close tags appear without our close tag first).
        // E.g. `<ul><li>a</ul>` — the `</ul>` ends the implicit `<li>`.
        if parser.match_str("</") {
            implicit_close = true;
            break;
        }

        if parser.match_str("<!--") {
            let c = super::comment::read_comment(parser)?;
            nodes.push(FragmentChild::Comment(c));
            continue;
        }
        if parser.match_str("<") {
            // HTML implicit close: if the next opening tag implicitly closes
            // *this* element (e.g. `<li>...<li>`), break out without
            // consuming. The parent's caller will close us. Mirrors
            // `closing_tag_omitted` in element.js:213-222.
            if let Some(next_name) = peek_opening_tag_name(parser.template, parser.index) {
                if crate::utils::element_names::closing_tag_omitted(tag_name, next_name) {
                    implicit_close = true;
                    break;
                }
            }
            let child = read_element_or_comment(parser)?;
            nodes.push(child);
            continue;
        }
        if parser.match_str("{") {
            // Block close markers (`{/if}`, `{/each}`, …) implicitly close
            // this element so the enclosing block can match its close —
            // mirrors upstream's `close()` recursive pop in tag.js:551-555.
            //
            // Block continuation markers (`{:else}`, `{:then}`, …) are
            // NOT auto-close triggers: an element open at the moment the
            // continuation appears is an error. Mirrors upstream's
            // `next()` final fall-through `e.block_invalid_continuation_placement`
            // at tag.js:536.
            //
            // Distinguish `{/if}` (close-block) from `{/* ... */}` (JS
            // block-comment inside an expression mustache) by requiring an
            // identifier letter after `/`.
            let rest = parser.template[parser.index + 1..].as_bytes();
            let mut iter = rest.iter().copied();
            let first = loop {
                match iter.next() {
                    Some(c) if c.is_ascii_whitespace() => continue,
                    other => break other,
                }
            };
            match first {
                Some(b':') => {
                    if matches!(iter.next(), Some(c) if c.is_ascii_alphabetic()) {
                        return Err(
                            svelte_diagnostics::errors::block_invalid_continuation_placement(
                                Some((parser.index as u32, parser.index as u32)),
                            ),
                        );
                    }
                }
                Some(b'/') => {
                    if matches!(iter.next(), Some(c) if c.is_ascii_alphabetic()) {
                        implicit_close = true;
                        break;
                    }
                }
                _ => {}
            }
            nodes.push(super::tag::read_tag(parser)?);
            continue;
        }
        let t = super::text::read_text(parser);
        nodes.push(FragmentChild::Text(t));
    }
    Ok((
        Fragment {
            nodes,
            metadata: Default::default(),
        },
        implicit_close,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    fn first_node(input: &str) -> FragmentChild {
        let r = parse(input, false).unwrap();
        r.fragment.nodes.into_iter().next().unwrap()
    }

    #[test]
    fn empty_div() {
        let n = first_node("<div></div>");
        match n {
            FragmentChild::RegularElement(el) => {
                assert_eq!(el.name, "div");
                assert_eq!(el.start, 0);
                assert_eq!(el.end, 11);
                assert!(el.attributes.is_empty());
                assert!(el.fragment.nodes.is_empty());
                assert_eq!(el.name_loc.start.line, 1);
                assert_eq!(el.name_loc.start.column, 1);
                assert_eq!(el.name_loc.start.character, Some(1));
                assert_eq!(el.name_loc.end.character, Some(4));
            }
            other => panic!("expected RegularElement, got {other:?}"),
        }
    }

    #[test]
    fn self_closing_br() {
        let n = first_node("<br/>");
        match n {
            FragmentChild::RegularElement(el) => {
                assert_eq!(el.name, "br");
                assert_eq!(el.end, 5);
                assert!(el.fragment.nodes.is_empty());
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn void_element_without_slash() {
        let n = first_node("<br>");
        match n {
            FragmentChild::RegularElement(el) => {
                assert_eq!(el.name, "br");
                assert_eq!(el.end, 4);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn element_with_text_child() {
        let n = first_node("<p>hello</p>");
        match n {
            FragmentChild::RegularElement(el) => {
                assert_eq!(el.name, "p");
                assert_eq!(el.fragment.nodes.len(), 1);
                match &el.fragment.nodes[0] {
                    FragmentChild::Text(t) => assert_eq!(t.raw, "hello"),
                    o => panic!("expected Text, got {o:?}"),
                }
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn nested_elements() {
        let n = first_node("<div><span>x</span></div>");
        let outer = match n {
            FragmentChild::RegularElement(el) => el,
            other => panic!("got {other:?}"),
        };
        assert_eq!(outer.name, "div");
        assert_eq!(outer.fragment.nodes.len(), 1);
        let inner = match &outer.fragment.nodes[0] {
            FragmentChild::RegularElement(el) => el,
            other => panic!("expected inner RegularElement, got {other:?}"),
        };
        assert_eq!(inner.name, "span");
        assert_eq!(inner.fragment.nodes.len(), 1);
    }

    #[test]
    fn bare_attribute() {
        let n = first_node("<input disabled>");
        match n {
            FragmentChild::RegularElement(el) => {
                assert_eq!(el.attributes.len(), 1);
                match &el.attributes[0] {
                    ElementAttribute::Attribute(a) => {
                        assert_eq!(a.name, "disabled");
                        assert!(matches!(a.value, AttributeValue::Empty));
                    }
                    other => panic!("expected Attribute, got {other:?}"),
                }
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn quoted_attribute_value() {
        let n = first_node(r#"<div class="foo bar"></div>"#);
        let el = match n {
            FragmentChild::RegularElement(e) => e,
            other => panic!("got {other:?}"),
        };
        assert_eq!(el.attributes.len(), 1);
        match &el.attributes[0] {
            ElementAttribute::Attribute(a) => {
                assert_eq!(a.name, "class");
                match &a.value {
                    AttributeValue::Many(parts) => {
                        assert_eq!(parts.len(), 1);
                        match &parts[0] {
                            AttributeValuePart::Text(t) => {
                                assert_eq!(t.raw, "foo bar");
                            }
                            o => panic!("expected Text, got {o:?}"),
                        }
                    }
                    o => panic!("expected Many, got {o:?}"),
                }
            }
            o => panic!("expected Attribute, got {o:?}"),
        }
    }

    #[test]
    fn multiple_attributes() {
        let n = first_node(r#"<input type="text" name="email" required>"#);
        match n {
            FragmentChild::RegularElement(el) => {
                assert_eq!(el.attributes.len(), 3);
            }
            other => panic!("got {other:?}"),
        }
    }
}
