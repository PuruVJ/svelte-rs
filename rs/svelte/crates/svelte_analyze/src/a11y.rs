//! A11y (accessibility) validation rules for RegularElement / SvelteElement.
//!
//! Ports the most common rules from
//! `packages/svelte/src/compiler/phases/2-analyze/visitors/shared/a11y/index.js`.
//! Each function emits a `CompileDiagnostic` warning when the rule fires.
//!
//! Current scope (most-impactful rules, expand as snapshot fixtures demand):
//! - `a11y_missing_attribute` — img/area/object/input[type=image] missing alt/title
//! - `a11y_distracting_elements` — `<marquee>` / `<blink>`
//! - `a11y_autofocus` — `autofocus` attribute on any focusable element
//! - `a11y_no_abstract_role` — abstract ARIA roles
//! - `a11y_no_redundant_roles` — `role` matches the implicit role
//! - `a11y_misplaced_scope` — `scope` on non-`<th>`
//! - `a11y_invalid_attribute` — `href="javascript:..."` etc.
//! - `a11y_label_has_associated_control` — `<label>` without nested control or `for`
//! - `a11y_click_events_have_key_events` — `onclick` without `onkeydown|onkeyup|onkeypress`
//! - `a11y_mouse_events_have_key_events` — `onmouseover` without `onfocus` (and `onmouseout` without `onblur`)
//! - `a11y_consider_explicit_label` — focusable element without accessible name

use svelte_ast::attributes::{Attribute, AttributeValue, AttributeValuePart, ElementAttribute};
use svelte_ast::elements::RegularElement;
use svelte_ast::fragment::{Fragment, FragmentChild};
use svelte_diagnostics::{warnings, CompileDiagnostic};

pub fn check_regular_element(el: &RegularElement) -> Vec<CompileDiagnostic> {
    let mut diags = Vec::new();
    let attrs: Vec<(&str, &AttributeValue)> = el
        .attributes
        .iter()
        .filter_map(|a| match a {
            ElementAttribute::Attribute(Attribute { name, value, .. }) => Some((name.as_str(), value)),
            _ => None,
        })
        .collect();
    // Legacy event directives (`on:click={...}`) — equivalent to `onclick={...}`.
    let directive_event_names: Vec<String> = el
        .attributes
        .iter()
        .filter_map(|a| match a {
            ElementAttribute::OnDirective(d) => Some(format!("on{}", d.name)),
            _ => None,
        })
        .collect();
    let has_event = |name: &str| -> bool {
        attrs.iter().any(|(n, _)| *n == name)
            || directive_event_names.iter().any(|s| s == name)
    };
    let span = Some((el.start, el.end));

    // 1. a11y_distracting_elements — `<marquee>` / `<blink>`
    if matches!(el.name.as_str(), "marquee" | "blink") {
        diags.push(warnings::a11y_distracting_elements(span, &el.name));
    }

    // 2. a11y_autofocus — any element with `autofocus` attribute
    if attrs.iter().any(|(n, _)| *n == "autofocus") {
        diags.push(warnings::a11y_autofocus(span));
    }

    // 3. a11y_misplaced_scope — `scope` only valid on `<th>`
    if el.name != "th" && attrs.iter().any(|(n, _)| *n == "scope") {
        diags.push(warnings::a11y_misplaced_scope(span));
    }

    // 4. a11y_missing_attribute — img/area/object/input[type=image]
    let attr_get = |key: &str| -> Option<&AttributeValue> {
        attrs.iter().find(|(n, _)| *n == key).map(|(_, v)| *v)
    };
    let has_attr = |k: &str| attr_get(k).is_some();
    match el.name.as_str() {
        "img" => {
            if !has_attr("alt") && !is_aria_hidden(&attrs) {
                diags.push(warnings::a11y_missing_attribute(
                    span, "img", "an", "alt",
                ));
            }
        }
        "area" => {
            if !has_attr("alt") && !has_attr("aria-label") && !has_attr("aria-labelledby") {
                diags.push(warnings::a11y_missing_attribute(
                    span,
                    "area",
                    "an",
                    "alt, aria-label or aria-labelledby",
                ));
            }
        }
        "object" => {
            if !has_attr("title") && !has_attr("aria-label") && !has_attr("aria-labelledby") {
                diags.push(warnings::a11y_missing_attribute(
                    span,
                    "object",
                    "a",
                    "title, aria-label or aria-labelledby",
                ));
            }
        }
        "input" => {
            let is_image_type = matches!(attr_static_string(attr_get("type")), Some(s) if s == "image");
            if is_image_type
                && !has_attr("alt")
                && !has_attr("aria-label")
                && !has_attr("aria-labelledby")
            {
                diags.push(warnings::a11y_missing_attribute(
                    span,
                    "input type=\"image\"",
                    "an",
                    "alt, aria-label or aria-labelledby",
                ));
            }
        }
        "html" => {
            if !has_attr("lang") {
                diags.push(warnings::a11y_missing_attribute(
                    span, "html", "a", "lang",
                ));
            }
        }
        "iframe" => {
            if !has_attr("title") {
                diags.push(warnings::a11y_missing_attribute(
                    span, "iframe", "a", "title",
                ));
            }
        }
        _ => {}
    }

    // 5. a11y_no_abstract_role — `role="<abstract>"` is invalid.
    if let Some(role) = attr_static_string(attr_get("role")) {
        if is_abstract_role(&role) {
            diags.push(warnings::a11y_no_abstract_role(span, &role));
        } else if is_redundant_role(&el.name, &role) {
            diags.push(warnings::a11y_no_redundant_roles(span, &role));
        }
    }

    // 6. a11y_invalid_attribute — `href="javascript:..."` / `href="#"`.
    if let Some(href) = attr_static_string(attr_get("href")) {
        if href.starts_with("javascript:") {
            diags.push(warnings::a11y_invalid_attribute(span, "href", &href));
        }
    }

    // 7. a11y_click_events_have_key_events — onclick without keyboard equivalent.
    let has_onclick = has_event("onclick");
    let has_key_event =
        has_event("onkeydown") || has_event("onkeyup") || has_event("onkeypress");
    if has_onclick && !has_key_event && is_interactive_role_eligible(&el.name) {
        diags.push(warnings::a11y_click_events_have_key_events(span));
    }

    // 8. a11y_mouse_events_have_key_events — onmouseover/out without focus/blur.
    for (mouse_evt, key_evt) in [("onmouseover", "onfocus"), ("onmouseout", "onblur")] {
        if has_event(mouse_evt) && !has_event(key_evt) {
            diags.push(warnings::a11y_mouse_events_have_key_events(
                span,
                mouse_evt,
                key_evt,
            ));
        }
    }

    // 9. a11y_label_has_associated_control — `<label>` without nested control or `for`.
    if el.name == "label" {
        let has_for = has_attr("for");
        let has_nested_control = fragment_has_form_control(&el.fragment);
        if !has_for && !has_nested_control {
            diags.push(warnings::a11y_label_has_associated_control(span));
        }
    }

    // 10. a11y_consider_explicit_label — `<button>` / `<a>` with no text/label.
    if matches!(el.name.as_str(), "button" | "a")
        && !has_attr("aria-label")
        && !has_attr("aria-labelledby")
        && !has_attr("title")
        && !is_aria_hidden(&attrs)
        && !has_attr("inert")
        && !fragment_has_text(&el.fragment)
    {
        diags.push(warnings::a11y_consider_explicit_label(span));
    }

    // 11. a11y_img_redundant_alt — `<img alt="image of ...">` / `<img alt="picture of ...">`.
    if el.name == "img" {
        if let Some(alt) = attr_static_string(attr_get("alt")) {
            let lower = alt.to_lowercase();
            if lower.contains("image of") || lower.contains("picture of") || lower.contains("photo of") {
                diags.push(warnings::a11y_img_redundant_alt(span));
            }
        }
    }

    // 12. a11y_missing_content — heading elements should have content.
    if matches!(el.name.as_str(), "h1" | "h2" | "h3" | "h4" | "h5" | "h6") {
        if !fragment_has_text(&el.fragment)
            && !has_attr("aria-label")
            && !has_attr("aria-labelledby")
        {
            diags.push(warnings::a11y_missing_content(span, &el.name));
        }
    }

    // 13. a11y_misplaced_role — `<html role="...">` is invalid.
    if el.name == "html" && has_attr("role") {
        diags.push(warnings::a11y_misplaced_role(span, "html"));
    }

    // 14. a11y_aria_attributes — these elements can't have aria-*.
    if matches!(el.name.as_str(), "meta" | "html" | "script" | "style") {
        for (n, _) in &attrs {
            if n.starts_with("aria-") || *n == "role" {
                diags.push(warnings::a11y_aria_attributes(span, &el.name));
                break;
            }
        }
    }

    // 15. a11y_role_supports_aria_props_implicit — `<img alt="" aria-disabled>` (skip for now)

    // 16. a11y_positive_tabindex — `tabindex >= 1` is bad.
    if let Some(tabindex) = attr_static_string(attr_get("tabindex")) {
        if let Ok(n) = tabindex.parse::<i32>() {
            if n > 0 {
                diags.push(warnings::a11y_positive_tabindex(span));
            }
        }
    }

    // 17. a11y_accesskey — `accesskey` attribute (avoid).
    if has_attr("accesskey") {
        diags.push(warnings::a11y_accesskey(span));
    }

    // 18. a11y_unknown_role — `role="foobar"` not in the WAI-ARIA list.
    if let Some(role) = attr_static_string(attr_get("role")) {
        if !is_known_role(&role) && !is_abstract_role(&role) {
            diags.push(warnings::a11y_unknown_role(span, &role, None));
        }
    }

    // 19. a11y_unknown_aria_attribute — `aria-foo=...` with foo not in spec.
    for (n, _) in &attrs {
        if let Some(stripped) = n.strip_prefix("aria-") {
            if !is_known_aria_attribute(stripped) {
                diags.push(warnings::a11y_unknown_aria_attribute(span, n, None));
            }
        }
    }

    // 20. a11y_media_has_caption — `<video>` without `<track kind="captions">`.
    if el.name == "video"
        && !has_attr("muted")
        && !fragment_has_caption_track(&el.fragment)
    {
        diags.push(warnings::a11y_media_has_caption(span));
    }

    // 21. a11y_figcaption_parent — `<figcaption>` must be a direct child of `<figure>`.
    //     (Approximated — we don't have parent context here. Skipped.)

    diags
}

/// WAI-ARIA 1.2 role list (subset — the most common roles). Mirrors
/// upstream's `aria-query`-backed check.
fn is_known_role(role: &str) -> bool {
    matches!(
        role,
        // Document structure
        "article" | "banner" | "complementary" | "contentinfo" | "definition"
        | "directory" | "document" | "feed" | "figure" | "form" | "group"
        | "heading" | "img" | "list" | "listitem" | "main" | "math" | "navigation"
        | "none" | "note" | "presentation" | "region" | "row" | "rowgroup"
        | "rowheader" | "search" | "separator" | "table" | "term" | "toolbar"
        | "tooltip"
        // Widget roles
        | "button" | "checkbox" | "combobox" | "gridcell" | "link" | "menuitem"
        | "menuitemcheckbox" | "menuitemradio" | "option" | "progressbar"
        | "radio" | "scrollbar" | "searchbox" | "slider" | "spinbutton"
        | "switch" | "tab" | "tabpanel" | "textbox" | "treeitem"
        // Composite widgets
        | "grid" | "listbox" | "menu" | "menubar" | "radiogroup"
        | "tablist" | "tree" | "treegrid"
        // Landmark
        | "application" | "alert" | "alertdialog" | "dialog" | "log" | "marquee"
        | "status" | "timer" | "columnheader"
        // Live regions
        | "code" | "deletion" | "emphasis" | "insertion" | "paragraph"
        | "strong" | "subscript" | "superscript" | "time" | "blockquote"
        | "caption" | "cell" | "generic" | "meter"
    )
}

/// WAI-ARIA 1.2 `aria-*` attribute list (subset).
fn is_known_aria_attribute(name: &str) -> bool {
    matches!(
        name,
        "activedescendant" | "atomic" | "autocomplete" | "busy" | "checked"
        | "colcount" | "colindex" | "colspan" | "controls" | "current"
        | "describedby" | "details" | "disabled" | "dropeffect" | "errormessage"
        | "expanded" | "flowto" | "grabbed" | "haspopup" | "hidden" | "invalid"
        | "keyshortcuts" | "label" | "labelledby" | "level" | "live" | "modal"
        | "multiline" | "multiselectable" | "orientation" | "owns" | "placeholder"
        | "posinset" | "pressed" | "readonly" | "relevant" | "required"
        | "roledescription" | "rowcount" | "rowindex" | "rowspan" | "selected"
        | "setsize" | "sort" | "valuemax" | "valuemin" | "valuenow" | "valuetext"
        | "braillelabel" | "brailleroledescription"
    )
}

fn fragment_has_caption_track(f: &Fragment) -> bool {
    for n in &f.nodes {
        if let FragmentChild::RegularElement(el) = n {
            if el.name == "track" {
                let kind = el.attributes.iter().find_map(|a| match a {
                    ElementAttribute::Attribute(Attribute { name, value, .. })
                        if name == "kind" =>
                    {
                        Some(value)
                    }
                    _ => None,
                });
                if let Some(v) = kind {
                    if let Some(s) = attr_static_string(Some(v)) {
                        if s == "captions" {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

/// Extract the static string value of an Attribute. Returns None for missing,
/// dynamic, or non-text values.
fn attr_static_string(value: Option<&AttributeValue>) -> Option<String> {
    match value? {
        AttributeValue::Empty(true) => Some(String::new()),
        AttributeValue::Empty(false) => None,
        AttributeValue::Many(parts) => {
            let mut text = String::new();
            for p in parts {
                match p {
                    AttributeValuePart::Text(t) => text.push_str(&t.data),
                    _ => return None,
                }
            }
            Some(text)
        }
        AttributeValue::Single(_) => None,
    }
}

fn is_aria_hidden(attrs: &[(&str, &AttributeValue)]) -> bool {
    let v = attrs
        .iter()
        .find(|(n, _)| *n == "aria-hidden")
        .map(|(_, v)| *v);
    match attr_static_string(v) {
        Some(s) => s == "true" || s.is_empty(),
        None => false,
    }
}

fn is_abstract_role(role: &str) -> bool {
    matches!(
        role,
        "command"
            | "composite"
            | "input"
            | "landmark"
            | "range"
            | "roletype"
            | "section"
            | "sectionhead"
            | "select"
            | "structure"
            | "widget"
            | "window"
    )
}

fn is_redundant_role(tag: &str, role: &str) -> bool {
    matches!(
        (tag, role),
        ("article", "article")
            | ("button", "button")
            | ("dialog", "dialog")
            | ("h1", "heading")
            | ("h2", "heading")
            | ("h3", "heading")
            | ("h4", "heading")
            | ("h5", "heading")
            | ("h6", "heading")
            | ("img", "img")
            | ("li", "listitem")
            | ("nav", "navigation")
            | ("ol", "list")
            | ("table", "table")
            | ("ul", "list")
    )
}

fn is_interactive_role_eligible(tag: &str) -> bool {
    !matches!(
        tag,
        "a" | "button"
            | "input"
            | "select"
            | "textarea"
            | "video"
            | "audio"
            | "details"
            | "summary"
    )
}

fn fragment_has_form_control(f: &Fragment) -> bool {
    for n in &f.nodes {
        match n {
            FragmentChild::RegularElement(el) => {
                if matches!(
                    el.name.as_str(),
                    "input" | "select" | "textarea" | "meter" | "progress" | "button"
                ) {
                    return true;
                }
                if fragment_has_form_control(&el.fragment) {
                    return true;
                }
            }
            FragmentChild::Component(c) => {
                if fragment_has_form_control(&c.fragment) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

fn fragment_has_text(f: &Fragment) -> bool {
    for n in &f.nodes {
        match n {
            FragmentChild::Text(t) if !t.data.trim().is_empty() => return true,
            FragmentChild::ExpressionTag(_) => return true,
            FragmentChild::RegularElement(el) => {
                if fragment_has_text(&el.fragment) {
                    return true;
                }
            }
            FragmentChild::Component(_) => return true, // assume slot content
            _ => {}
        }
    }
    false
}
