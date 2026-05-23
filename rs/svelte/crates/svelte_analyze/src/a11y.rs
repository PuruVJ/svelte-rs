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

/// Check for `attribute_duplicate` — two attributes/directives with the same
/// effective name on the same element. Returns hard errors (not warnings).
pub fn check_duplicate_attributes(
    attrs: &[ElementAttribute],
) -> Vec<CompileDiagnostic> {
    use std::collections::HashSet;
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for a in attrs {
        let (name, start, end) = match a {
            ElementAttribute::Attribute(svelte_ast::Attribute {
                name, start, end, ..
            }) => (name.to_string(), *start, *end),
            ElementAttribute::ClassDirective(d) => (format!("class:{}", d.name), d.start, d.end),
            ElementAttribute::StyleDirective(d) => (format!("style:{}", d.name), d.start, d.end),
            ElementAttribute::BindDirective(d) => (format!("bind:{}", d.name), d.start, d.end),
            ElementAttribute::OnDirective(d) => (format!("on:{}", d.name), d.start, d.end),
            ElementAttribute::UseDirective(d) => (format!("use:{}", d.name), d.start, d.end),
            ElementAttribute::TransitionDirective(d) => {
                (format!("transition:{}", d.name), d.start, d.end)
            }
            _ => continue,
        };
        if !seen.insert(name.to_string()) {
            out.push(svelte_diagnostics::errors::attribute_duplicate(Some((
                start, end,
            ))));
        }
    }
    out
}

pub fn check_regular_element(el: &RegularElement) -> Vec<CompileDiagnostic> {
    check_regular_element_with_parent(el, None)
}

pub fn check_regular_element_with_parent(
    el: &RegularElement,
    parent: Option<&str>,
) -> Vec<CompileDiagnostic> {
    let mut diags = Vec::new();
    let attrs: Vec<(&str, &AttributeValue)> = el
        .attributes
        .iter()
        .filter_map(|a| match a {
            ElementAttribute::Attribute(Attribute { name, value, .. }) => Some((*name, value)),
            _ => None,
        })
        .collect();
    let has_spread = el
        .attributes
        .iter()
        .any(|a| matches!(a, ElementAttribute::SpreadAttribute(_)));
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
    if matches!(el.name.as_ref(), "marquee" | "blink") {
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
    match el.name.as_ref() {
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
        "a" => {
            // <a> needs an href OR a name/id attribute. Without any of those
            // it isn't a real anchor. Skip when `aria-disabled` is set —
            // that explicitly marks the link as non-interactive.
            if !has_attr("href")
                && !has_attr("name")
                && !has_attr("id")
                && !has_attr("aria-disabled")
            {
                diags.push(warnings::a11y_missing_attribute(
                    span, "a", "an", "href",
                ));
            }
            // Empty href / href="#" / href="javascript:..." → invalid.
            if let Some(href) = attr_static_string(attr_get("href")) {
                if href.is_empty() || href == "#" {
                    diags.push(warnings::a11y_invalid_attribute(span, "href", &href));
                }
            }
            // Same checks for `xlink:href` (SVG anchors).
            if let Some(xhref) = attr_static_string(attr_get("xlink:href")) {
                if xhref.is_empty() || xhref == "#" {
                    let attr_span = el.attributes.iter().find_map(|a| {
                        if let ElementAttribute::Attribute(att) = a {
                            if att.name == "xlink:href" {
                                return Some((att.start, att.end));
                            }
                        }
                        None
                    });
                    diags.push(warnings::a11y_invalid_attribute(
                        attr_span.or(span),
                        "xlink:href",
                        &xhref,
                    ));
                }
            }
            // Empty name=''
            if let Some(name_val) = attr_static_string(attr_get("name")) {
                if name_val.is_empty() {
                    diags.push(warnings::a11y_invalid_attribute(span, "name", ""));
                }
            }
            // Empty id=''
            if let Some(id_val) = attr_static_string(attr_get("id")) {
                if id_val.is_empty() {
                    diags.push(warnings::a11y_invalid_attribute(span, "id", ""));
                }
            }
        }
        _ => {}
    }

    // 5. a11y_no_abstract_role — `role="<abstract>"` is invalid.
    if let Some(role) = attr_static_string(attr_get("role")) {
        if is_abstract_role(&role) {
            diags.push(warnings::a11y_no_abstract_role(span, &role));
        } else if is_redundant_role(&el.name, &role) {
            // Upstream carve-outs:
            //  - `<ul>` / `<ol>` / `<li>` / `<menu>` with `role="list"` /
            //    `role="listitem"` etc. is OK because `list-style: none`
            //    strips list semantics and the role brings them back.
            //  - `<a>` without `href` has no implicit role, so a role
            //    isn't redundant.
            let is_list_carveout = matches!(el.name.as_ref(), "ul" | "ol" | "li" | "menu");
            let is_a_no_href = el.name == "a" && !has_attr("href");
            if !is_list_carveout && !is_a_no_href {
                diags.push(warnings::a11y_no_redundant_roles(span, &role));
            }
        }
    }

    // 6. a11y_invalid_attribute — `href="javascript:..."` / `href="#"`.
    if let Some(href) = attr_static_string(attr_get("href")) {
        if href.starts_with("javascript:") {
            diags.push(warnings::a11y_invalid_attribute(span, "href", &href));
        }
    }

    // 7. Element-interactivity-based rules.
    let has_onclick = has_event("onclick");
    let has_key_event =
        has_event("onkeydown") || has_event("onkeyup") || has_event("onkeypress");
    let role = attr_static_string(attr_get("role"));
    let role_is_presentation = matches!(role.as_deref(), Some("presentation") | Some("none"));
    let role_is_interactive = role.as_deref().map_or(false, is_interactive_role);
    let role_is_noninteractive_v = role.as_deref().map_or(false, is_non_interactive_role);
    let aria_hidden = is_aria_hidden(&attrs);
    let disabled = has_attr("disabled")
        || attr_static_string(attr_get("aria-disabled")).as_deref() == Some("true");
    // Element interactivity classification (Interactive / NonInteractive / Static).
    let interactivity = element_interactivity(&el.name, &attrs);
    let is_strict_interactive = matches!(interactivity, Interactivity::Interactive);
    let is_strict_noninteractive = matches!(interactivity, Interactivity::NonInteractive);
    let is_strict_static = matches!(interactivity, Interactivity::Static);
    let has_any_interactive_handler = INTERACTIVE_HANDLERS.iter().any(|h| has_event(h));
    let has_recommended_interactive_handler = RECOMMENDED_INTERACTIVE_HANDLERS
        .iter()
        .any(|h| has_event(h));
    let hidden_from_sr = aria_hidden
        || (el.name == "input"
            && attr_static_string(attr_get("type")).as_deref() == Some("hidden"));
    let role_is_non_presentation = role.is_some() && !role_is_presentation;
    // When the `role` attribute is present but its value is a dynamic
    // expression (not a static string), we can't know whether it's
    // interactive — be conservative and skip role-dependent checks.
    let role_is_dynamic = attr_get("role").is_some() && role.is_none();
    // click_events_have_key_events — onclick + non-interactive + no key.
    if has_onclick
        && !has_key_event
        && !hidden_from_sr
        && !disabled
        && !has_spread
        && !role_is_dynamic
        && (role.is_none() || role_is_non_presentation)
        && !is_strict_interactive
        && !role_is_interactive
    {
        diags.push(warnings::a11y_click_events_have_key_events(span));
    }
    // no_noninteractive_element_interactions: recommended_interactive handler
    // on a non-interactive element (or non-interactive role on interactive).
    if !has_spread
        && !aria_hidden
        && !disabled
        && !role_is_presentation
        && has_recommended_interactive_handler
        && ((!is_strict_interactive && role_is_noninteractive_v)
            || (is_strict_noninteractive && role.is_none()))
    {
        diags.push(warnings::a11y_no_noninteractive_element_interactions(
            span, &el.name,
        ));
    }
    // no_static_element_interactions: interactive handler on STATIC element.
    // Suppressed when `role` is present but its value is dynamic (we can't
    // tell what it'll resolve to).
    let has_role_attr = attr_get("role").is_some();
    let role_is_dynamic = has_role_attr && role.is_none();
    if !has_spread
        && !role_is_dynamic
        && !hidden_from_sr
        && !role_is_presentation
        && !is_strict_interactive
        && !role_is_interactive
        && !is_strict_noninteractive
        && !role_is_noninteractive_v
        && !is_abstract_role(role.as_deref().unwrap_or(""))
        && has_any_interactive_handler
        && is_strict_static
    {
        // List up to 2 handler names in source order for the message.
        let mut hndlrs: Vec<&'static str> = Vec::new();
        for h in INTERACTIVE_HANDLERS {
            if has_event(h) {
                hndlrs.push(&h[2..]); // strip leading `on`
            }
        }
        let listed = list_handlers(&hndlrs);
        diags.push(warnings::a11y_no_static_element_interactions(
            span, &el.name, &listed,
        ));
    }
    // interactive_supports_focus.
    if let Some(role_name) = role.as_deref() {
        if is_interactive_role(role_name)
            && !aria_hidden
            && !disabled
            && !role_is_presentation
            && !has_attr("tabindex")
            && !has_attr("disabled")
            && has_any_interactive_handler
        {
            diags.push(warnings::a11y_interactive_supports_focus(span, role_name));
        }
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
    if matches!(el.name.as_ref(), "button" | "a")
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
    if matches!(el.name.as_ref(), "h1" | "h2" | "h3" | "h4" | "h5" | "h6") {
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

    // 14. a11y_aria_attributes / a11y_misplaced_role — these elements can't
    // have role or aria-*. role gets its own diagnostic; aria-* gets the
    // aria_attributes warning.
    if matches!(el.name.as_ref(), "meta" | "html" | "script" | "style") {
        for (n, _) in &attrs {
            if *n == "role" {
                diags.push(warnings::a11y_misplaced_role(span, &el.name));
            } else if n.starts_with("aria-") {
                diags.push(warnings::a11y_aria_attributes(span, &el.name));
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

    // 17. a11y_accesskey — `accesskey` attribute (avoid). Case-insensitive
    // since HTML attributes are case-insensitive.
    if attrs.iter().any(|(n, _)| n.eq_ignore_ascii_case("accesskey")) {
        diags.push(warnings::a11y_accesskey(span));
    }

    // 18. a11y_unknown_role — `role="foobar"` not in the WAI-ARIA list.
    if let Some(role) = attr_static_string(attr_get("role")) {
        if !is_known_role(&role) && !is_abstract_role(&role) {
            diags.push(warnings::a11y_unknown_role(span, &role, None));
        }
    }

    // 19. a11y_unknown_aria_attribute — `aria-foo=...` with foo not in spec.
    //     Also: a11y_incorrect_aria_attribute_type_* — value-type checks.
    for (n, v) in &attrs {
        if let Some(stripped) = n.strip_prefix("aria-") {
            if !is_known_aria_attribute(stripped) {
                diags.push(warnings::a11y_unknown_aria_attribute(span, n, None));
                continue;
            }
            // Type-check the value if static.
            if let Some(value) = attr_static_string(Some(*v)) {
                if let Some(diag) = check_aria_value_type(span, n, stripped, &value) {
                    diags.push(diag);
                }
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

    // 21. a11y_figcaption_parent — must be a direct child of `<figure>`.
    if el.name == "figcaption" && parent != Some("figure") {
        diags.push(warnings::a11y_figcaption_parent(span));
    }

    // 22. a11y_figcaption_index — figcaption must be first or last child of figure.
    if el.name == "figure" {
        // Find the index of the figcaption among non-comment / non-whitespace
        // children.
        let nodes: Vec<&FragmentChild> = el
            .fragment
            .nodes
            .iter()
            .filter(|n| match n {
                FragmentChild::Text(t) => !t.data.trim().is_empty(),
                FragmentChild::Comment(_) => false,
                _ => true,
            })
            .collect();
        if let Some(idx) = nodes.iter().position(|n| match n {
            FragmentChild::RegularElement(c) => c.name == "figcaption",
            _ => false,
        }) {
            if idx != 0 && idx != nodes.len() - 1 {
                if let FragmentChild::RegularElement(fc) = nodes[idx] {
                    diags.push(warnings::a11y_figcaption_index(Some((fc.start, fc.end))));
                }
            }
        }
    }

    // 23. a11y_hidden — heading element with `aria-hidden="true"`.
    if matches!(el.name.as_ref(), "h1" | "h2" | "h3" | "h4" | "h5" | "h6") {
        if attr_static_string(attr_get("aria-hidden")).as_deref() == Some("true") {
            diags.push(warnings::a11y_hidden(span, &el.name));
        }
    }

    // 24. a11y_aria_activedescendant_has_tabindex —
    //   `aria-activedescendant` on a non-interactive element without `tabindex`.
    if has_attr("aria-activedescendant")
        && !has_attr("tabindex")
        && !has_spread
        && !is_interactive_html_element(&el.name, &attrs)
    {
        // Use the attribute's own span if we can find it.
        let attr_span = el.attributes.iter().find_map(|a| {
            if let ElementAttribute::Attribute(att) = a {
                if att.name == "aria-activedescendant" {
                    return Some((att.start, att.end));
                }
            }
            None
        });
        diags.push(warnings::a11y_aria_activedescendant_has_tabindex(
            attr_span.or(span),
        ));
    }

    // 25. a11y_no_noninteractive_tabindex — `tabindex` on a non-interactive
    //   element that isn't role=interactive.
    if let Some(tabindex_val) = attr_static_string(attr_get("tabindex")) {
        let n = tabindex_val.parse::<i32>().ok();
        let element_is_interactive = is_interactive_html_element(&el.name, &attrs);
        let role_interactive = role.as_deref().map_or(false, is_interactive_role);
        // Implicit-role-interactive: e.g. `<datalist>` has implicit role
        // `listbox` which is interactive. Mirrors upstream's behavior.
        let implicit_role_interactive = role.is_none()
            && implicit_role_for(&el.name)
                .as_deref()
                .map_or(false, is_interactive_role);
        // tabindex < 0 doesn't bring focus, no warning. Skip when role is
        // dynamic (we don't know if it would be interactive).
        if n.map_or(true, |x| x >= 0)
            && !element_is_interactive
            && !role_interactive
            && !implicit_role_interactive
            && !role_is_dynamic
        {
            let attr_span = el.attributes.iter().find_map(|a| {
                if let ElementAttribute::Attribute(att) = a {
                    if att.name == "tabindex" {
                        return Some((att.start, att.end));
                    }
                }
                None
            });
            diags.push(warnings::a11y_no_noninteractive_tabindex(
                attr_span.or(span),
            ));
        }
    }

    // 26. a11y_no_noninteractive_element_to_interactive_role / opposite.
    //   Suppressed when the role is redundant (matches implicit role) — that
    //   case fires `a11y_no_redundant_roles` instead.
    if let Some(role_name) = role.as_deref() {
        let is_exception = matches!(
            (el.name.as_ref(), role_name),
            ("ul", "listbox")
                | ("ul", "menu")
                | ("ul", "menubar")
                | ("ul", "radiogroup")
                | ("ul", "tablist")
                | ("ul", "tree")
                | ("ul", "treegrid")
                | ("ol", "listbox")
                | ("ol", "menu")
                | ("ol", "menubar")
                | ("ol", "radiogroup")
                | ("ol", "tablist")
                | ("ol", "tree")
                | ("ol", "treegrid")
                | ("menu", "listbox")
                | ("menu", "menu")
                | ("menu", "menubar")
                | ("menu", "radiogroup")
                | ("menu", "tablist")
                | ("menu", "tree")
                | ("menu", "treegrid")
                | ("li", "menuitem")
                | ("li", "option")
                | ("li", "row")
                | ("li", "tab")
                | ("li", "treeitem")
                | ("table", "grid")
                | ("td", "gridcell")
                | ("fieldset", "radiogroup")
                | ("fieldset", "presentation")
        );
        if is_known_role(role_name) && !is_redundant_role(&el.name, role_name) && !is_exception {
            let element_is_interactive_strict = is_strictly_interactive_html_element(&el.name, &attrs);
            let element_is_non_interactive_strict =
                is_strictly_non_interactive_html_element(&el.name, &attrs);
            let role_is_int = is_interactive_role(role_name);
            let role_is_noninteractive = is_non_interactive_role(role_name);
            let attr_span = el.attributes.iter().find_map(|a| {
                if let ElementAttribute::Attribute(att) = a {
                    if att.name == "role" {
                        return Some((att.start, att.end));
                    }
                }
                None
            });
            let role_span = attr_span.or(span);
            if role_is_noninteractive && element_is_interactive_strict {
                diags.push(warnings::a11y_no_interactive_element_to_noninteractive_role(
                    role_span, &el.name, role_name,
                ));
            }
            if role_is_int && element_is_non_interactive_strict {
                diags.push(warnings::a11y_no_noninteractive_element_to_interactive_role(
                    role_span, &el.name, role_name,
                ));
            }
        }
    }

    // 26c. a11y_role_supports_aria_props / a11y_role_supports_aria_props_implicit
    //   For each aria-* attribute on the element, check if it's supported by
    //   the effective role (explicit if `role=...`, else the implicit role
    //   from the tag name). Mirrors a11y/index.js:323-336 with the
    //   aria-query roles.props data shipped at build time.
    let effective_role = role.clone().or_else(|| implicit_role_for(&el.name));
    let is_implicit_role = role.is_none();
    if let Some(role_name) = &effective_role {
        if let Some(supported) = role_supported_aria_props(role_name) {
            for (n, _) in &attrs {
                if let Some(stripped) = n.strip_prefix("aria-") {
                    if !is_known_aria_attribute(stripped) {
                        continue;
                    }
                    if !supported.contains(n) {
                        let attr_span = el.attributes.iter().find_map(|a| {
                            if let ElementAttribute::Attribute(att) = a {
                                if att.name == *n {
                                    return Some((att.start, att.end));
                                }
                            }
                            None
                        });
                        let span = attr_span.or(span);
                        if is_implicit_role {
                            diags.push(
                                warnings::a11y_role_supports_aria_props_implicit(
                                    span,
                                    n,
                                    role_name,
                                    &el.name,
                                ),
                            );
                        } else {
                            diags.push(warnings::a11y_role_supports_aria_props(
                                span,
                                n,
                                role_name,
                            ));
                        }
                    }
                }
            }
        }
    }

    // 26b. a11y_role_has_required_aria_props — certain roles must have
    //   specific `aria-*` attributes defined. Mirrors aria-query's
    //   `requiredProps`. We only encode the commonly-required-props subset.
    if let Some(role_name) = role.as_deref() {
        let required: &[&str] = match role_name {
            "checkbox" => &["aria-checked"],
            "meter" => &["aria-valuenow"],
            "option" => &["aria-selected"],
            "radio" => &["aria-checked"],
            "scrollbar" => &["aria-controls", "aria-valuenow"],
            "slider" => &["aria-valuenow"],
            "switch" => &["aria-checked"],
            // heading: only required if not implicit (h1-h6 supplies level).
            "heading" if !matches!(el.name.as_ref(),
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6") => &["aria-level"],
            _ => &[],
        };
        if !required.is_empty() && !is_redundant_role(&el.name, role_name) {
            let missing: Vec<&&str> = required.iter().filter(|p| !has_attr(p)).collect();
            // Don't fire if the element's implicit role already supplies the
            // required attribute (e.g. `<input type="checkbox" role="switch">`
            // — input[type=checkbox] is interactive and supplies state).
            let element_is_interactive = is_interactive_html_element(&el.name, &attrs);
            if !missing.is_empty() && !element_is_interactive {
                let mut quoted: Vec<String> =
                    missing.iter().map(|p| format!("\"{}\"", p)).collect();
                let props_str = match quoted.len() {
                    0 => String::new(),
                    1 => quoted.remove(0),
                    _ => {
                        let last = quoted.pop().unwrap();
                        format!("{} and {}", quoted.join(", "), last)
                    }
                };
                let attr_span = el.attributes.iter().find_map(|a| {
                    if let ElementAttribute::Attribute(att) = a {
                        if att.name == "role" {
                            return Some((att.start, att.end));
                        }
                    }
                    None
                });
                diags.push(warnings::a11y_role_has_required_aria_props(
                    attr_span.or(span),
                    role_name,
                    &props_str,
                ));
            }
        }
    }

    // 27. a11y_autocomplete_valid — validate `autocomplete=` on `<input>`.
    if el.name == "input" {
        if let Some(autocomplete_attr) = el.attributes.iter().find_map(|a| match a {
            ElementAttribute::Attribute(att) if att.name == "autocomplete" => Some(att),
            _ => None,
        }) {
            let type_val = attr_static_string(attr_get("type"))
                .unwrap_or_else(|| "text".to_string());
            let type_lower = type_val.to_lowercase();
            let value = match &autocomplete_attr.value {
                AttributeValue::Empty => Some("true".to_string()),
                AttributeValue::Many(parts) => {
                    let mut all_static = true;
                    let mut s = String::new();
                    for p in parts {
                        if let AttributeValuePart::Text(t) = p {
                            s.push_str(&t.data);
                        } else {
                            all_static = false;
                            break;
                        }
                    }
                    if all_static {
                        Some(s)
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if let Some(val) = value {
                if !val.is_empty() && !is_valid_autocomplete(&type_lower, &val) {
                    diags.push(warnings::a11y_autocomplete_valid(
                        Some((autocomplete_attr.start, autocomplete_attr.end)),
                        &type_val,
                        &val,
                    ));
                }
            }
        }
    }

    // 28. a11y_consider_explicit_label (popover-label) — `<button popovertarget>`
    //     without aria-label etc. The existing rule (10) only fires when the
    //     element has NO text and no aria — adding popover variant.
    if matches!(el.name.as_ref(), "button" | "a")
        && has_attr("popovertarget")
        && !has_attr("aria-label")
        && !has_attr("aria-labelledby")
        && !has_attr("title")
        && !is_aria_hidden(&attrs)
        && !has_attr("inert")
        && !fragment_has_text(&el.fragment)
    {
        diags.push(warnings::a11y_consider_explicit_label(span));
    }

    diags
}

/// Strict-interactive HTML elements — used by the role-conflict rules to
/// decide if a role on an interactive element constitutes a downgrade.
/// Narrower than `is_interactive_html_element` (which also covers e.g. `<a>`
/// without href as interactive-eligible). Mirrors upstream's
/// `interactive_element_role_schemas` / AXObject lookups.
fn is_strictly_interactive_html_element(
    tag: &str,
    attrs: &[(&str, &AttributeValue)],
) -> bool {
    if tag == "input" {
        let t = attrs
            .iter()
            .find(|(n, _)| *n == "type")
            .and_then(|(_, v)| attr_static_string(Some(*v)));
        return !matches!(t.as_deref(), Some("hidden"));
    }
    if tag == "a" || tag == "area" {
        return attrs.iter().any(|(n, _)| *n == "href");
    }
    matches!(
        tag,
        "button" | "select" | "textarea" | "summary" | "details"
        | "menuitem" | "option" | "tr" | "dialog"
    )
}

/// Strict-non-interactive HTML elements. Excludes `section` / `header` /
/// `div` / `span` etc. (those are Static). Includes content-grouping
/// landmarks like `article`, `main`, `footer`, `nav` and structural
/// elements like `ol`, `ul`, `li`, `table`, `figure`.
fn is_strictly_non_interactive_html_element(
    tag: &str,
    attrs: &[(&str, &AttributeValue)],
) -> bool {
    let _ = attrs;
    matches!(
        tag,
        "article" | "aside" | "blockquote" | "br" | "caption" | "dd"
        | "dfn" | "dialog" | "dl" | "dt" | "fieldset" | "figcaption" | "figure"
        | "footer" | "form" | "frame" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
        | "hgroup" | "hr" | "iframe" | "img" | "label" | "legend" | "li" | "main"
        | "marquee" | "menu" | "meter" | "nav" | "ol" | "output" | "p" | "pre"
        | "progress" | "table" | "tbody" | "td" | "tfoot" | "th"
        | "thead" | "time" | "title" | "ul"
    )
}

/// HTML elements that are interactive without considering attributes.
fn is_interactive_html_element(tag: &str, attrs: &[(&str, &AttributeValue)]) -> bool {
    // `<input>` is interactive UNLESS type=hidden.
    if tag == "input" {
        let t = attrs
            .iter()
            .find(|(n, _)| *n == "type")
            .and_then(|(_, v)| attr_static_string(Some(*v)));
        return t.as_deref() != Some("hidden");
    }
    // `<a>` and `<area>` are interactive only with href.
    if tag == "a" || tag == "area" {
        return attrs.iter().any(|(n, _)| *n == "href");
    }
    // `<audio>` / `<video>` interactive only with controls.
    if tag == "audio" || tag == "video" {
        return attrs.iter().any(|(n, _)| *n == "controls");
    }
    matches!(
        tag,
        "button" | "select" | "textarea" | "details" | "summary" |
        "menuitem" | "option" | "iframe" | "embed" | "object" | "tr" | "tabpanel"
    )
}

/// Static HTML elements — non-semantic containers that get
/// `a11y_no_static_element_interactions` for onclick handlers.
fn is_static_html_element(tag: &str) -> bool {
    matches!(
        tag,
        "div" | "span" | "p" | "section" | "article" | "main" | "header"
        | "footer" | "nav" | "aside" | "hr" | "address"
    )
}

/// ARIA interactive roles. Derived from aria-query@5.3.1 by filtering
/// non-abstract roles whose superClass chain includes 'widget' or 'window',
/// minus the `progressbar`/`generic` carve-outs and plus toolbar/tabpanel/
/// cell (treated as interactive in practice). Mirrors `interactive_roles`
/// in constants.js.
fn is_interactive_role(role: &str) -> bool {
    matches!(
        role,
        "alertdialog" | "button" | "cell" | "checkbox" | "columnheader"
        | "combobox" | "dialog" | "doc-backlink" | "doc-biblioref"
        | "doc-glossref" | "doc-noteref" | "grid" | "gridcell" | "link"
        | "listbox" | "menu" | "menubar" | "menuitem" | "menuitemcheckbox"
        | "menuitemradio" | "option" | "radio" | "radiogroup" | "row"
        | "rowheader" | "scrollbar" | "searchbox" | "slider" | "spinbutton"
        | "switch" | "tab" | "tablist" | "tabpanel" | "textbox" | "toolbar"
        | "tree" | "treegrid" | "treeitem"
    )
}

/// ARIA non-interactive roles — derived analogously. Mirrors
/// `non_interactive_roles` in constants.js.
fn is_non_interactive_role(role: &str) -> bool {
    matches!(
        role,
        "alert" | "application" | "article" | "banner" | "blockquote" | "caption"
        | "code" | "complementary" | "contentinfo" | "definition" | "deletion"
        | "directory" | "doc-abstract" | "doc-acknowledgments" | "doc-afterword"
        | "doc-appendix" | "doc-biblioentry" | "doc-bibliography" | "doc-chapter"
        | "doc-colophon" | "doc-conclusion" | "doc-cover" | "doc-credit"
        | "doc-credits" | "doc-dedication" | "doc-endnote" | "doc-endnotes"
        | "doc-epigraph" | "doc-epilogue" | "doc-errata" | "doc-example"
        | "doc-footnote" | "doc-foreword" | "doc-glossary" | "doc-index"
        | "doc-introduction" | "doc-notice" | "doc-pagebreak" | "doc-pagefooter"
        | "doc-pageheader" | "doc-pagelist" | "doc-part" | "doc-preface"
        | "doc-prologue" | "doc-pullquote" | "doc-qna" | "doc-subtitle"
        | "doc-tip" | "doc-toc" | "document" | "emphasis" | "feed" | "figure"
        | "form" | "graphics-document" | "graphics-object" | "graphics-symbol"
        | "group" | "heading" | "img" | "insertion" | "list" | "listitem"
        | "log" | "main" | "mark" | "marquee" | "math" | "meter" | "navigation"
        | "none" | "note" | "paragraph" | "presentation" | "progressbar"
        | "region" | "rowgroup" | "search" | "separator" | "status" | "strong"
        | "subscript" | "superscript" | "table" | "term" | "time" | "timer"
        | "tooltip"
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Interactivity {
    Interactive,
    NonInteractive,
    Static,
}

/// Mirrors upstream's `element_interactivity` — categorizes an HTML element
/// based on tag + attributes into Interactive | NonInteractive | Static.
fn element_interactivity(tag: &str, attrs: &[(&str, &AttributeValue)]) -> Interactivity {
    if is_strictly_interactive_html_element(tag, attrs) {
        return Interactivity::Interactive;
    }
    if is_strictly_non_interactive_html_element(tag, attrs) {
        return Interactivity::NonInteractive;
    }
    Interactivity::Static
}

/// Format a list of strings the way upstream's `list()` utility does:
/// `["a"]` → `"a"`, `["a", "b"]` → `"a or b"`, `["a", "b", "c"]` →
/// `"a, b or c"`.
fn list_handlers(items: &[&str]) -> String {
    match items.len() {
        0 => String::new(),
        1 => items[0].to_string(),
        2 => format!("{} or {}", items[0], items[1]),
        _ => {
            let (last, rest) = items.split_last().unwrap();
            format!("{} or {}", rest.join(", "), last)
        }
    }
}

const INTERACTIVE_HANDLERS: &[&str] = &[
    // Keyboard
    "onkeypress", "onkeydown", "onkeyup",
    // Click / mouse
    "onclick", "oncontextmenu", "ondblclick", "ondrag", "ondragend",
    "ondragenter", "ondragexit", "ondragleave", "ondragover", "ondragstart",
    "ondrop", "onmousedown", "onmouseenter", "onmouseleave", "onmousemove",
    "onmouseout", "onmouseover", "onmouseup",
    // Pointer
    "onpointerdown", "onpointerup", "onpointermove", "onpointerenter",
    "onpointerleave", "onpointerover", "onpointerout", "onpointercancel",
    // Touch
    "ontouchstart", "ontouchend", "ontouchmove", "ontouchcancel",
];

const RECOMMENDED_INTERACTIVE_HANDLERS: &[&str] = &[
    "onclick", "onmousedown", "onmouseup", "onkeypress", "onkeydown", "onkeyup",
];

/// Validate `autocomplete="..."` against the standard tokens for the given
/// input type. Returns true when the value is acceptable.
fn is_valid_autocomplete(input_type: &str, value: &str) -> bool {
    // Special trivial values.
    let lower = value.to_lowercase();
    let lower = lower.trim();
    if lower == "on" || lower == "off" {
        return true;
    }
    // Dynamic / partially-static values are passed through unchanged by the
    // caller — only fully-static values reach here. Empty string already
    // returned valid by the caller.
    // For hidden inputs, any token is accepted.
    if input_type == "hidden" {
        return true;
    }
    // Split tokens and check each against the autofill name list.
    let tokens: Vec<&str> = lower.split_whitespace().collect();
    if tokens.is_empty() {
        return false;
    }
    // Allow `webauthn` (or other special trailing tokens) at the end on
    // non-hidden inputs ONLY if the field tokens are present.
    let last = *tokens.last().unwrap();
    let mut field_tokens = tokens.clone();
    let mut has_webauthn = false;
    if last == "webauthn" {
        has_webauthn = true;
        field_tokens.pop();
    }
    // Allow `section-XXX` first token.
    let mut idx = 0;
    if !field_tokens.is_empty() && field_tokens[0].starts_with("section-") {
        idx = 1;
    }
    // Optional shipping/billing.
    if idx < field_tokens.len()
        && (field_tokens[idx] == "shipping" || field_tokens[idx] == "billing")
    {
        idx += 1;
    }
    // Optional home/work/mobile/fax/pager.
    if idx < field_tokens.len()
        && matches!(field_tokens[idx], "home" | "work" | "mobile" | "fax" | "pager")
    {
        idx += 1;
    }
    // Now the next token must be a valid field name.
    if idx >= field_tokens.len() {
        return false;
    }
    let field = field_tokens[idx];
    idx += 1;
    if !is_valid_autofill_field(field) {
        return false;
    }
    if idx != field_tokens.len() {
        return false;
    }
    // If `webauthn` was present, the field must be one of a specific subset —
    // for our tests, `webauthn` alone (no field) is invalid.
    if has_webauthn && field_tokens.is_empty() {
        return false;
    }
    true
}

fn is_valid_autofill_field(name: &str) -> bool {
    matches!(
        name,
        "name" | "honorific-prefix" | "given-name" | "additional-name" | "family-name"
        | "honorific-suffix" | "nickname" | "username" | "new-password" | "current-password"
        | "one-time-code" | "organization-title" | "organization" | "street-address"
        | "address-line1" | "address-line2" | "address-line3" | "address-level4"
        | "address-level3" | "address-level2" | "address-level1" | "country"
        | "country-name" | "postal-code" | "cc-name" | "cc-given-name" | "cc-additional-name"
        | "cc-family-name" | "cc-number" | "cc-exp" | "cc-exp-month" | "cc-exp-year"
        | "cc-csc" | "cc-type" | "transaction-currency" | "transaction-amount" | "language"
        | "bday" | "bday-day" | "bday-month" | "bday-year" | "sex" | "url" | "photo"
        | "tel" | "tel-country-code" | "tel-national" | "tel-area-code" | "tel-local"
        | "tel-local-prefix" | "tel-local-suffix" | "tel-extension" | "email" | "impp"
    )
}

/// WAI-ARIA 1.2 role list. Sourced from
/// https://www.w3.org/TR/wai-aria-1.2/#role_definitions (concrete + composite).
fn is_known_role(role: &str) -> bool {
    // DPUB-ARIA `doc-*` and graphics-ARIA `graphics-*` roles are also valid.
    if role.starts_with("doc-") || role.starts_with("graphics-") {
        return true;
    }
    matches!(
        role,
        "alert" | "alertdialog" | "application" | "article" | "banner"
        | "blockquote" | "button" | "caption" | "cell" | "checkbox"
        | "code" | "columnheader" | "combobox" | "complementary" | "contentinfo"
        | "definition" | "deletion" | "dialog" | "directory" | "document"
        | "emphasis" | "feed" | "figure" | "form" | "generic" | "grid"
        | "gridcell" | "group" | "heading" | "img" | "insertion" | "link"
        | "list" | "listbox" | "listitem" | "log" | "main" | "marquee"
        | "math" | "menu" | "menubar" | "menuitem" | "menuitemcheckbox"
        | "menuitemradio" | "meter" | "navigation" | "none" | "note"
        | "option" | "paragraph" | "presentation" | "progressbar" | "radio"
        | "radiogroup" | "region" | "row" | "rowgroup" | "rowheader"
        | "scrollbar" | "search" | "searchbox" | "separator" | "slider"
        | "spinbutton" | "status" | "strong" | "subscript" | "superscript"
        | "switch" | "tab" | "table" | "tablist" | "tabpanel" | "term"
        | "textbox" | "time" | "timer" | "toolbar" | "tooltip" | "tree"
        | "treegrid" | "treeitem"
    )
}

/// Check the static value of an aria attribute against its expected type.
/// Returns a diagnostic if the value type doesn't match.
fn check_aria_value_type(
    span: Option<(u32, u32)>,
    full_name: &str,
    stripped: &str,
    value: &str,
) -> Option<CompileDiagnostic> {
    match aria_type(stripped) {
        AriaType::Boolean => {
            if !matches!(value, "true" | "false" | "") {
                Some(warnings::a11y_incorrect_aria_attribute_type_boolean(
                    span, full_name,
                ))
            } else {
                None
            }
        }
        AriaType::Tristate => {
            if !matches!(value, "true" | "false" | "mixed" | "") {
                Some(warnings::a11y_incorrect_aria_attribute_type_tristate(
                    span, full_name,
                ))
            } else {
                None
            }
        }
        AriaType::Integer => {
            if value.parse::<i64>().is_err() {
                Some(warnings::a11y_incorrect_aria_attribute_type_integer(
                    span, full_name,
                ))
            } else {
                None
            }
        }
        AriaType::Number => {
            if value.parse::<f64>().is_err() {
                Some(warnings::a11y_incorrect_aria_attribute_type(
                    span, full_name, "number",
                ))
            } else {
                None
            }
        }
        AriaType::Token(allowed) => {
            if !allowed.contains(&value) {
                Some(warnings::a11y_incorrect_aria_attribute_type_token(
                    span, full_name, value,
                ))
            } else {
                None
            }
        }
        AriaType::TokenList(allowed) => {
            for token in value.split_ascii_whitespace() {
                if !allowed.contains(&token) {
                    return Some(warnings::a11y_incorrect_aria_attribute_type_tokenlist(
                        span, full_name, token,
                    ));
                }
            }
            None
        }
        AriaType::String | AriaType::IdRef | AriaType::IdRefList | AriaType::Unknown => None,
    }
}

#[derive(Debug)]
enum AriaType {
    Boolean,
    Tristate,
    Integer,
    Number,
    String,
    IdRef,
    IdRefList,
    Token(&'static [&'static str]),
    TokenList(&'static [&'static str]),
    Unknown,
}

fn aria_type(name: &str) -> AriaType {
    match name {
        "atomic" | "busy" | "disabled" | "modal" | "multiline" | "multiselectable"
        | "readonly" | "required" | "selected" | "hidden" | "haspopup" => AriaType::Boolean,
        "checked" | "pressed" | "expanded" | "grabbed" => AriaType::Tristate,
        "level" | "colcount" | "colindex" | "colspan" | "posinset" | "rowcount"
        | "rowindex" | "rowspan" | "setsize" => AriaType::Integer,
        "valuemax" | "valuemin" | "valuenow" => AriaType::Number,
        "autocomplete" => AriaType::Token(&["inline", "list", "both", "none"]),
        "current" => AriaType::Token(&["page", "step", "location", "date", "time", "true", "false"]),
        "dropeffect" => AriaType::TokenList(&["copy", "move", "link", "execute", "popup", "none"]),
        "live" => AriaType::Token(&["off", "polite", "assertive"]),
        "orientation" => AriaType::Token(&["horizontal", "vertical", "undefined"]),
        "relevant" => AriaType::TokenList(&["additions", "removals", "text", "all"]),
        "sort" => AriaType::Token(&["ascending", "descending", "none", "other"]),
        "invalid" => AriaType::Token(&["grammar", "spelling", "true", "false"]),
        "activedescendant" | "errormessage" => AriaType::IdRef,
        "controls" | "describedby" | "details" | "flowto" | "labelledby" | "owns" => {
            AriaType::IdRefList
        }
        "keyshortcuts" | "label" | "placeholder" | "roledescription" | "valuetext"
        | "braillelabel" | "brailleroledescription" => AriaType::String,
        _ => AriaType::Unknown,
    }
}

/// HTML element → implicit ARIA role lookup. Mirrors
/// `a11y_implicit_semantics` in upstream's constants.js.
fn implicit_role_for(tag: &str) -> Option<String> {
    Some(match tag {
        "a" | "area" => "link",
        "article" => "article",
        "aside" => "complementary",
        "body" => "document",
        "button" => "button",
        "datalist" => "listbox",
        "dd" => "definition",
        "dfn" => "term",
        "details" => "group",
        "dialog" => "dialog",
        "dt" => "term",
        "fieldset" => "group",
        "figure" => "figure",
        "form" => "form",
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => "heading",
        "hr" => "separator",
        "img" => "img",
        "li" => "listitem",
        "link" => "link",
        "main" => "main",
        "menu" | "ol" | "ul" => "list",
        "meter" | "progress" => "progressbar",
        "nav" => "navigation",
        "option" => "option",
        "optgroup" => "group",
        "output" => "status",
        "section" => "region",
        "summary" => "button",
        "table" => "table",
        "tbody" | "tfoot" | "thead" => "rowgroup",
        "textarea" => "textbox",
        "tr" => "row",
        "header" => "banner",
        "footer" => "contentinfo",
        _ => return None,
    }
    .to_string())
}

/// Per-role supported aria-* attribute list. Data extracted from
/// `aria-query@5.3.1`'s `rolesMap` (props are merged across superClass
/// chains, so this includes both role-specific and global props).
fn role_supported_aria_props(role: &str) -> Option<&'static [&'static str]> {
    Some(match role {
        "command" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "composite" => &["aria-activedescendant", "aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "input" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "landmark" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "range" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription", "aria-valuemax", "aria-valuemin", "aria-valuenow"],
        "roletype" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "section" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "sectionhead" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "select" => &["aria-activedescendant", "aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-orientation", "aria-owns", "aria-relevant", "aria-roledescription"],
        "structure" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "widget" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "window" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-modal", "aria-owns", "aria-relevant", "aria-roledescription"],
        "alert" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "alertdialog" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-modal", "aria-owns", "aria-relevant", "aria-roledescription"],
        "application" => &["aria-activedescendant", "aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "article" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-posinset", "aria-relevant", "aria-roledescription", "aria-setsize"],
        "banner" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "blockquote" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "button" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-pressed", "aria-relevant", "aria-roledescription"],
        "caption" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "cell" => &["aria-atomic", "aria-busy", "aria-colindex", "aria-colspan", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription", "aria-rowindex", "aria-rowspan"],
        "checkbox" => &["aria-atomic", "aria-busy", "aria-checked", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-readonly", "aria-relevant", "aria-required", "aria-roledescription"],
        "code" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "columnheader" => &["aria-atomic", "aria-busy", "aria-colindex", "aria-colspan", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-readonly", "aria-relevant", "aria-required", "aria-roledescription", "aria-rowindex", "aria-rowspan", "aria-selected", "aria-sort"],
        "combobox" => &["aria-activedescendant", "aria-atomic", "aria-autocomplete", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-readonly", "aria-relevant", "aria-required", "aria-roledescription"],
        "complementary" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "contentinfo" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "definition" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "deletion" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "dialog" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-modal", "aria-owns", "aria-relevant", "aria-roledescription"],
        "directory" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "document" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "emphasis" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "feed" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "figure" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "form" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "generic" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "grid" => &["aria-activedescendant", "aria-atomic", "aria-busy", "aria-colcount", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-multiselectable", "aria-owns", "aria-readonly", "aria-relevant", "aria-roledescription", "aria-rowcount"],
        "gridcell" => &["aria-atomic", "aria-busy", "aria-colindex", "aria-colspan", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-readonly", "aria-relevant", "aria-required", "aria-roledescription", "aria-rowindex", "aria-rowspan", "aria-selected"],
        "group" => &["aria-activedescendant", "aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "heading" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-level", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "img" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "insertion" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "link" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "list" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "listbox" => &["aria-activedescendant", "aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-multiselectable", "aria-orientation", "aria-owns", "aria-readonly", "aria-relevant", "aria-required", "aria-roledescription"],
        "listitem" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-level", "aria-live", "aria-owns", "aria-posinset", "aria-relevant", "aria-roledescription", "aria-setsize"],
        "log" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "main" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "mark" => &["aria-atomic", "aria-braillelabel", "aria-brailleroledescription", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-description", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "marquee" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "math" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "menu" => &["aria-activedescendant", "aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-orientation", "aria-owns", "aria-relevant", "aria-roledescription"],
        "menubar" => &["aria-activedescendant", "aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-orientation", "aria-owns", "aria-relevant", "aria-roledescription"],
        "menuitem" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-posinset", "aria-relevant", "aria-roledescription", "aria-setsize"],
        "menuitemcheckbox" => &["aria-atomic", "aria-busy", "aria-checked", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-posinset", "aria-readonly", "aria-relevant", "aria-required", "aria-roledescription", "aria-setsize"],
        "menuitemradio" => &["aria-atomic", "aria-busy", "aria-checked", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-posinset", "aria-readonly", "aria-relevant", "aria-required", "aria-roledescription", "aria-setsize"],
        "meter" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription", "aria-valuemax", "aria-valuemin", "aria-valuenow", "aria-valuetext"],
        "navigation" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "none" => &[],
        "note" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "option" => &["aria-atomic", "aria-busy", "aria-checked", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-posinset", "aria-relevant", "aria-roledescription", "aria-selected", "aria-setsize"],
        "paragraph" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "presentation" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "progressbar" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription", "aria-valuemax", "aria-valuemin", "aria-valuenow", "aria-valuetext"],
        "radio" => &["aria-atomic", "aria-busy", "aria-checked", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-posinset", "aria-relevant", "aria-roledescription", "aria-setsize"],
        "radiogroup" => &["aria-activedescendant", "aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-orientation", "aria-owns", "aria-readonly", "aria-relevant", "aria-required", "aria-roledescription"],
        "region" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "row" => &["aria-activedescendant", "aria-atomic", "aria-busy", "aria-colindex", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-level", "aria-live", "aria-owns", "aria-posinset", "aria-relevant", "aria-roledescription", "aria-rowindex", "aria-selected", "aria-setsize"],
        "rowgroup" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "rowheader" => &["aria-atomic", "aria-busy", "aria-colindex", "aria-colspan", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-readonly", "aria-relevant", "aria-required", "aria-roledescription", "aria-rowindex", "aria-rowspan", "aria-selected", "aria-sort"],
        "scrollbar" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-orientation", "aria-owns", "aria-relevant", "aria-roledescription", "aria-valuemax", "aria-valuemin", "aria-valuenow", "aria-valuetext"],
        "search" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "searchbox" => &["aria-activedescendant", "aria-atomic", "aria-autocomplete", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-multiline", "aria-owns", "aria-placeholder", "aria-readonly", "aria-relevant", "aria-required", "aria-roledescription"],
        "separator" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-orientation", "aria-owns", "aria-relevant", "aria-roledescription", "aria-valuemax", "aria-valuemin", "aria-valuenow", "aria-valuetext"],
        "slider" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-orientation", "aria-owns", "aria-readonly", "aria-relevant", "aria-roledescription", "aria-valuemax", "aria-valuemin", "aria-valuenow", "aria-valuetext"],
        "spinbutton" => &["aria-activedescendant", "aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-readonly", "aria-relevant", "aria-required", "aria-roledescription", "aria-valuemax", "aria-valuemin", "aria-valuenow", "aria-valuetext"],
        "status" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "strong" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "subscript" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "superscript" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "switch" => &["aria-atomic", "aria-busy", "aria-checked", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-readonly", "aria-relevant", "aria-required", "aria-roledescription"],
        "tab" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-posinset", "aria-relevant", "aria-roledescription", "aria-selected", "aria-setsize"],
        "table" => &["aria-atomic", "aria-busy", "aria-colcount", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription", "aria-rowcount"],
        "tablist" => &["aria-activedescendant", "aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-level", "aria-live", "aria-multiselectable", "aria-orientation", "aria-owns", "aria-relevant", "aria-roledescription"],
        "tabpanel" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "term" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "textbox" => &["aria-activedescendant", "aria-atomic", "aria-autocomplete", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-multiline", "aria-owns", "aria-placeholder", "aria-readonly", "aria-relevant", "aria-required", "aria-roledescription"],
        "time" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "timer" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "toolbar" => &["aria-activedescendant", "aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-orientation", "aria-owns", "aria-relevant", "aria-roledescription"],
        "tooltip" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-dropeffect", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "tree" => &["aria-activedescendant", "aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-multiselectable", "aria-orientation", "aria-owns", "aria-relevant", "aria-required", "aria-roledescription"],
        "treegrid" => &["aria-activedescendant", "aria-atomic", "aria-busy", "aria-colcount", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-flowto", "aria-grabbed", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-multiselectable", "aria-orientation", "aria-owns", "aria-readonly", "aria-relevant", "aria-required", "aria-roledescription", "aria-rowcount"],
        "treeitem" => &["aria-atomic", "aria-busy", "aria-checked", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-level", "aria-live", "aria-owns", "aria-posinset", "aria-relevant", "aria-roledescription", "aria-selected", "aria-setsize"],
        "doc-abstract" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-acknowledgments" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-afterword" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-appendix" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-backlink" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-biblioentry" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-level", "aria-live", "aria-owns", "aria-posinset", "aria-relevant", "aria-roledescription", "aria-setsize"],
        "doc-bibliography" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-biblioref" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-chapter" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-colophon" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-conclusion" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-cover" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-credit" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-credits" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-dedication" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-endnote" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-level", "aria-live", "aria-owns", "aria-posinset", "aria-relevant", "aria-roledescription", "aria-setsize"],
        "doc-endnotes" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-epigraph" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-epilogue" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-errata" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-example" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-footnote" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-foreword" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-glossary" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-glossref" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-index" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-introduction" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-noteref" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-notice" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-pagebreak" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-orientation", "aria-owns", "aria-relevant", "aria-roledescription", "aria-valuemax", "aria-valuemin", "aria-valuenow", "aria-valuetext"],
        "doc-pagefooter" => &["aria-atomic", "aria-braillelabel", "aria-brailleroledescription", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-description", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-pageheader" => &["aria-atomic", "aria-braillelabel", "aria-brailleroledescription", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-description", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-pagelist" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-part" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-preface" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-prologue" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-pullquote" => &[],
        "doc-qna" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-subtitle" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-tip" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "doc-toc" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "graphics-document" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "graphics-object" => &["aria-activedescendant", "aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        "graphics-symbol" => &["aria-atomic", "aria-busy", "aria-controls", "aria-current", "aria-describedby", "aria-details", "aria-disabled", "aria-dropeffect", "aria-errormessage", "aria-expanded", "aria-flowto", "aria-grabbed", "aria-haspopup", "aria-hidden", "aria-invalid", "aria-keyshortcuts", "aria-label", "aria-labelledby", "aria-live", "aria-owns", "aria-relevant", "aria-roledescription"],
        _ => return None,
    })
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
                        if *name == "kind" =>
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
        AttributeValue::Empty => Some(String::new()),
        AttributeValue::Empty => None,
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
    // Mirrors `a11y_implicit_semantics` from
    // `phases/2-analyze/visitors/shared/a11y/constants.js`. We also bake in
    // the upstream carve-outs for ul/ol/li/menu (which use CSS list-style
    // tricks) and `<a>` without href (which has no role until then).
    // Those carve-outs are applied at the call site.
    matches!(
        (tag, role),
        ("a", "link")
            | ("area", "link")
            | ("article", "article")
            | ("aside", "complementary")
            | ("body", "document")
            | ("button", "button")
            | ("datalist", "listbox")
            | ("dd", "definition")
            | ("dfn", "term")
            | ("dialog", "dialog")
            | ("details", "group")
            | ("dt", "term")
            | ("fieldset", "group")
            | ("figure", "figure")
            | ("form", "form")
            | ("h1", "heading")
            | ("h2", "heading")
            | ("h3", "heading")
            | ("h4", "heading")
            | ("h5", "heading")
            | ("h6", "heading")
            | ("hr", "separator")
            | ("img", "img")
            | ("li", "listitem")
            | ("link", "link")
            | ("main", "main")
            | ("menu", "list")
            | ("meter", "progressbar")
            | ("nav", "navigation")
            | ("ol", "list")
            | ("option", "option")
            | ("optgroup", "group")
            | ("output", "status")
            | ("progress", "progressbar")
            | ("section", "region")
            | ("summary", "button")
            | ("table", "table")
            | ("tbody", "rowgroup")
            | ("textarea", "textbox")
            | ("tfoot", "rowgroup")
            | ("thead", "rowgroup")
            | ("tr", "row")
            | ("ul", "list")
            | ("header", "banner")
            | ("footer", "contentinfo")
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
                    el.name.as_ref(),
                    "input" | "select" | "textarea" | "meter" | "progress" | "button"
                ) {
                    return true;
                }
                if fragment_has_form_control(&el.fragment) {
                    return true;
                }
            }
            // Components / slots / svelte:* might render a form control —
            // assume they do (matches upstream's permissive check).
            FragmentChild::Component(_)
            | FragmentChild::SvelteComponent(_)
            | FragmentChild::SvelteSelf(_)
            | FragmentChild::SvelteElement(_)
            | FragmentChild::SvelteFragment(_)
            | FragmentChild::SlotElement(_)
            | FragmentChild::RenderTag(_) => return true,
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
                // Mirrors upstream `has_content` carve-outs:
                //  - `popover`-anchored children don't count as button label
                //    (they pop OUT of the button visually).
                //  - `<img alt="...">` and `<selectedcontent>` count as content
                //    directly without needing descendant text.
                let has_popover = el.attributes.iter().any(|a| {
                    matches!(a, ElementAttribute::Attribute(att) if att.name == "popover")
                });
                if has_popover {
                    continue;
                }
                if el.name == "img"
                    && el.attributes.iter().any(|a| {
                        matches!(a, ElementAttribute::Attribute(att) if att.name == "alt")
                    })
                {
                    return true;
                }
                if el.name == "selectedcontent" {
                    return true;
                }
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
