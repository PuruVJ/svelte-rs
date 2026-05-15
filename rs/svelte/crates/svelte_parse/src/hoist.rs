//! Post-parse hoist of `<script>` and `<style>` elements.
//!
//! After the fragment parser builds a flat list of children (with `<script>`
//! and `<style>` as `RegularElement` placeholders), this pass walks the
//! root fragment and:
//!
//! - For each `<script>`: extracts the body text, parses it with OXC as a
//!   full `Program`, builds a `Script` AST node, and moves it to
//!   `Root.instance` or `Root.module` depending on the script's `context`.
//! - For each `<style>`: removes it from the fragment. (Phase 2g will
//!   parse the CSS body and set `Root.css`. For now `Root.css` stays `None`.)
//!
//! Mirrors how upstream's parser populates `Root.instance/module/css` at the
//! end of `phases/1-parse/index.js::parse`. Mid-fragment scripts/styles
//! (technically invalid) are left in place — upstream is also lenient here.
//!
//! Body bounds are recovered by rescanning the source: the open tag's `>`
//! is located via `find_open_tag_end` (quote- and brace-aware so attribute
//! values like `class="a>b"` or `disabled={x>5}` don't confuse the scan),
//! and the close tag's `<` is the last `<` in the source before the
//! element's end.

use serde_json::Value;
use svelte_ast::{
    Attribute, AttributeValue, AttributeValuePart, ElementAttribute,
    FragmentChild, JsComment, JsCommentKind, Position, RegularElement, Root, Script,
    ScriptContext, ScriptKind, SourceLocation, SvelteOptions, SvelteOptionsRaw,
};
use svelte_diagnostics::CompileDiagnostic;

use crate::oxc_bridge::{parse_program, RawComment};
use crate::utils::locator::LineMap;

/// Detect `<script>` and `<style>` children of `root.fragment` and hoist
/// them onto `root`. Mutates in place.
pub fn hoist_scripts_and_styles(
    root: &mut Root,
    source: &str,
    line_map: &LineMap,
    ts: bool,
) -> Result<(), CompileDiagnostic> {
    // Pass 1: hoist <svelte:options>.
    let mut i = 0;
    while i < root.fragment.nodes.len() {
        let is_options = matches!(
            &root.fragment.nodes[i],
            FragmentChild::SvelteOptions(_)
        );
        if !is_options {
            i += 1;
            continue;
        }
        let node = root.fragment.nodes.remove(i);
        let raw = match node {
            FragmentChild::SvelteOptions(r) => r,
            _ => unreachable!(),
        };
        root.options = Some(build_options(raw));
    }

    // Pass 2: hoist <script> / <style>.
    let mut i = 0;
    while i < root.fragment.nodes.len() {
        let is_script_or_style = match &root.fragment.nodes[i] {
            FragmentChild::RegularElement(el) => el.name == "script" || el.name == "style",
            _ => false,
        };
        if !is_script_or_style {
            i += 1;
            continue;
        }
        // Per upstream `element.js:323-350`, when the script element is at
        // the top level of the root fragment, the parser looks backwards
        // through fragment.nodes for the immediately-preceding HTML Comment
        // (skipping whitespace-only Text nodes that touch the script's
        // start position). That comment's text is later attached to the
        // resulting Program's `leadingComments` so the `svelte-ignore`
        // warning mechanism can find it.
        let preceding_html_comment = find_preceding_html_comment(&root.fragment.nodes, i);

        let node = root.fragment.nodes.remove(i);
        let el = match node {
            FragmentChild::RegularElement(el) => el,
            _ => unreachable!(),
        };
        if el.name == "script" {
            let (mut script, comments) = build_script(el, source, line_map, ts)?;
            if let Some(comment_data) = preceding_html_comment {
                if let Value::Object(prog) = &mut script.content {
                    let entry = serde_json::json!({
                        "type": "Line",
                        "value": comment_data,
                    });
                    let leading = prog
                        .entry("leadingComments".to_string())
                        .or_insert_with(|| Value::Array(Vec::new()));
                    if let Value::Array(arr) = leading {
                        // Per upstream, this replaces rather than appends — the
                        // root's leadingComments is reserved for this single
                        // svelte-ignore marker.
                        arr.clear();
                        arr.push(entry);
                    }
                }
            }
            let _ = ts; // not currently used for build_script; ts is detected per-element.
            // Add the script's comments to `Root.comments`. Mirrors how
            // upstream's `read_script` passes `parser.root.comments` to
            // acorn's `onComment`, accumulating into the root list.
            for c in comments {
                root.comments.push(raw_comment_to_js(&c, line_map));
            }
            if is_module_script(&script) {
                root.module = Some(script);
            } else {
                root.instance = Some(script);
            }
        } else {
            // `<style>` — parse the body via svelte_css_parser and populate
            // `Root.css`. Mirrors `read_style` in style.js:25-46.
            let el_start = el.start;
            let body_start = find_open_tag_end(source, el_start as usize);
            let preceding_comment = find_preceding_comment_node(&root.fragment.nodes, i);
            let (stylesheet, _) = svelte_css_parser::read_style(
                source,
                el_start,
                body_start,
                el.attributes.clone(),
                preceding_comment,
            )?;
            root.css = Some(stylesheet);
        }
    }
    Ok(())
}

/// Find the immediately-preceding HTML `Comment` node (skipping over
/// whitespace-only Text nodes). Returns a clone of the comment so it can be
/// attached as `StyleSheet.content.comment`. Mirrors the comment-attachment
/// upstream does for `<style>` elements.
fn find_preceding_comment_node(
    nodes: &[FragmentChild],
    idx: usize,
) -> Option<svelte_ast::Comment> {
    let mut j = idx;
    while j > 0 {
        j -= 1;
        match &nodes[j] {
            FragmentChild::Text(t) if t.raw.trim().is_empty() => continue,
            FragmentChild::Comment(c) => return Some(c.clone()),
            _ => return None,
        }
    }
    None
}

fn build_script(
    el: RegularElement,
    source: &str,
    line_map: &LineMap,
    _ts_outer: bool,
) -> Result<(Script, Vec<RawComment>), CompileDiagnostic> {
    let el_start = el.start as usize;
    let el_end = el.end as usize;

    let body_start = find_open_tag_end(source, el_start);
    let body_end = source[..el_end].rfind('<').unwrap_or(body_start);

    // Decide TS-ness from the `<script lang="ts">` attribute.
    let ts = attribute_string_value(&el.attributes, "lang")
        .map(|v| v == "ts" || v == "typescript")
        .unwrap_or(false);

    // Convert the attribute list to `Vec<Attribute>` (drop any non-Attribute
    // entries like directives — they're not valid on `<script>`).
    let attributes: Vec<Attribute> = el
        .attributes
        .into_iter()
        .filter_map(|a| match a {
            ElementAttribute::Attribute(a) => Some(a),
            _ => None,
        })
        .collect();

    let (mut content, raw_comments) = if body_end > body_start {
        parse_program(source, line_map, body_start, body_end, ts)?
    } else {
        // Empty <script></script> still emits an empty Program node.
        (empty_program(body_start, body_end, line_map), Vec::new())
    };

    // Mirror upstream `read_script.js:36-43`. Two overrides:
    //   - `Program.start` and `Program.end` are set to the script BODY
    //     bounds (acorn's defaults skip leading whitespace, which we don't
    //     want — `ast.start = script_start` in upstream).
    //   - `Program.loc.start/end` are set from the ELEMENT bounds (the
    //     `<` of `<script>` and the position just past `</script>`).
    // The numeric byte offsets and the line/col are intentionally
    // inconsistent here for sourcemap reasons.
    override_program_position(
        &mut content,
        line_map,
        body_start,
        body_end,
        el_start,
        el_end,
    );

    let context = if has_module_marker(&attributes) {
        ScriptContext::Module
    } else {
        ScriptContext::Default
    };

    Ok((
        Script {
            kind: ScriptKind::Script,
            start: el.start,
            end: el.end,
            context,
            content,
            attributes,
        },
        raw_comments,
    ))
}

/// Convert a `RawComment` (from OXC) into a `JsComment` for `Root.comments`.
fn raw_comment_to_js(c: &RawComment, line_map: &LineMap) -> JsComment {
    let (sl, sc) = line_map.locate(c.start as usize);
    let (el, ec) = line_map.locate(c.end as usize);
    JsComment {
        kind: if c.line {
            JsCommentKind::Line
        } else {
            JsCommentKind::Block
        },
        value: c.value.clone(),
        start: c.start,
        end: c.end,
        loc: SourceLocation {
            start: Position {
                line: sl,
                column: sc,
                character: None,
            },
            end: Position {
                line: el,
                column: ec,
                character: None,
            },
        },
    }
}

/// `<script>` is a module script if it has a bare `module` attribute, or a
/// `context="module"` attribute (the Svelte 4 form).
fn is_module_script(s: &Script) -> bool {
    matches!(s.context, ScriptContext::Module)
}

fn has_module_marker(attrs: &[Attribute]) -> bool {
    for a in attrs {
        if a.name == "module" {
            return true;
        }
        if a.name == "context" {
            if let Some(v) = attribute_value_as_str(&a.value) {
                if v == "module" {
                    return true;
                }
            }
        }
    }
    false
}

fn attribute_string_value<'a>(
    attrs: &'a [ElementAttribute],
    name: &str,
) -> Option<&'a str> {
    attrs.iter().find_map(|a| match a {
        ElementAttribute::Attribute(a) if a.name == name => attribute_value_as_str(&a.value),
        _ => None,
    })
}

fn attribute_value_as_str(value: &AttributeValue) -> Option<&str> {
    match value {
        AttributeValue::Many(parts) if parts.len() == 1 => match &parts[0] {
            AttributeValuePart::Text(t) => Some(&t.data),
            _ => None,
        },
        _ => None,
    }
}

/// Apply the upstream `read_script` overrides to the Program node:
/// numeric `start/end` use the script body bounds, while `loc.start/end`
/// use the element bounds (the `<` of `<script>` and end of `</script>`).
fn override_program_position(
    program: &mut Value,
    line_map: &LineMap,
    body_start: usize,
    body_end: usize,
    el_start: usize,
    el_end: usize,
) {
    let map = match program.as_object_mut() {
        Some(m) => m,
        None => return,
    };
    map.insert("start".to_string(), Value::from(body_start));
    map.insert("end".to_string(), Value::from(body_end));
    let (sl, sc) = line_map.locate(el_start);
    let (el, ec) = line_map.locate(el_end);
    map.insert(
        "loc".to_string(),
        serde_json::json!({
            "start": { "line": sl, "column": sc },
            "end":   { "line": el, "column": ec }
        }),
    );
}

fn empty_program(start: usize, end: usize, line_map: &LineMap) -> Value {
    serde_json::json!({
        "type": "Program",
        "start": start,
        "end": end,
        "loc": {
            "start": pos(start, line_map),
            "end": pos(end, line_map),
        },
        "body": [],
        "sourceType": "module"
    })
}

fn pos(offset: usize, line_map: &LineMap) -> Value {
    let (line, column) = line_map.locate(offset);
    serde_json::json!({ "line": line, "column": column })
}

/// Convert a parsed `<svelte:options ...>` element into the hoisted
/// `Root.options` shape. Mirrors `phases/1-parse/read/options.js` for the
/// common cases. Specifically extracts:
///   - `runes` → `Option<bool>` (from a boolean Literal expression).
///   - `customElement="tag-name"` → `Some({ tag: "tag-name" })`.
///
/// Other options recognized by upstream (`accessors`, `immutable`,
/// `preserveWhitespace`, `namespace`, `css`, `customElement` with an object
/// expression) are not yet extracted but are still preserved through the
/// `attributes` array. Errors that upstream raises (invalid customElement
/// form, deprecated `tag`, etc.) are not yet emitted.
fn build_options(raw: SvelteOptionsRaw) -> SvelteOptions {
    let attributes: Vec<Attribute> = raw
        .attributes
        .iter()
        .filter_map(|a| match a {
            ElementAttribute::Attribute(a) => Some(a.clone()),
            _ => None,
        })
        .collect();

    let mut options = SvelteOptions {
        start: raw.start,
        end: raw.end,
        runes: None,
        immutable: None,
        accessors: None,
        preserve_whitespace: None,
        namespace: None,
        css: None,
        custom_element: None,
        attributes: attributes.clone(),
    };

    for attribute in &attributes {
        match attribute.name.as_str() {
            "runes" => {
                options.runes = boolean_value(&attribute.value);
            }
            "customElement" => {
                if let Some(tag) = static_string_value(&attribute.value) {
                    options.custom_element = Some(serde_json::json!({ "tag": tag }));
                }
                // Object-expression form deferred.
            }
            _ => {
                // Other options not yet wired.
            }
        }
    }

    options
}

/// Extract a static boolean from an attribute value like `runes={true}`.
fn boolean_value(value: &AttributeValue) -> Option<bool> {
    match value {
        AttributeValue::Empty(b) => Some(*b),
        AttributeValue::Single(expr_tag) => match expr_tag.expression.get("type")?.as_str()? {
            "Literal" => expr_tag.expression.get("value")?.as_bool(),
            _ => None,
        },
        AttributeValue::Many(parts) if parts.len() == 1 => match &parts[0] {
            AttributeValuePart::ExpressionTag(t) => match t.expression.get("type")?.as_str()? {
                "Literal" => t.expression.get("value")?.as_bool(),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

/// Extract a static string from an attribute value like `customElement="x"`.
fn static_string_value(value: &AttributeValue) -> Option<String> {
    match value {
        AttributeValue::Many(parts) if parts.len() == 1 => match &parts[0] {
            AttributeValuePart::Text(t) => Some(t.data.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// Mirror of upstream's check in `element.js:323-341`: walking backwards
/// from `script_idx`, find the most recent `Comment` node whose `end` is
/// exactly the position the script starts at (after skipping
/// whitespace-only `Text` nodes that touch the script). Returns the
/// comment's `data` if such a comment exists.
fn find_preceding_html_comment(nodes: &[FragmentChild], script_idx: usize) -> Option<String> {
    if script_idx == 0 {
        return None;
    }
    let script_start = match &nodes[script_idx] {
        FragmentChild::RegularElement(el) => el.start,
        _ => return None,
    };
    let mut last_end = script_start;
    for i in (0..script_idx).rev() {
        match &nodes[i] {
            FragmentChild::Comment(c) => {
                if c.end == last_end {
                    return Some(c.data.clone());
                }
                return None;
            }
            FragmentChild::Text(t) => {
                // Skip if whitespace-only and adjacent.
                if t.end == last_end && t.data.trim().is_empty() {
                    last_end = t.start;
                    continue;
                }
                return None;
            }
            _ => return None,
        }
    }
    None
}

/// Walk an element's opening tag starting at `el_start` (the `<`) and return
/// the offset just past the `>` (or `/>`) that closes the open tag.
///
/// Tracks balanced quotes and `{...}` so attribute values containing `>` —
/// e.g. `class="a>b"`, `disabled={x > 5}` — don't confuse the scan.
pub fn find_open_tag_end(source: &str, el_start: usize) -> usize {
    let bytes = source.as_bytes();
    let mut i = el_start + 1; // past `<`
    let mut in_quote: Option<u8> = None;
    let mut brace_depth: u32 = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = in_quote {
            if b == q {
                in_quote = None;
            }
            i += 1;
            continue;
        }
        if brace_depth > 0 {
            match b {
                b'{' => brace_depth += 1,
                b'}' => brace_depth -= 1,
                _ => {}
            }
            i += 1;
            continue;
        }
        match b {
            b'"' | b'\'' => in_quote = Some(b),
            b'{' => brace_depth = 1,
            b'/' if bytes.get(i + 1).copied() == Some(b'>') => return i + 2,
            b'>' => return i + 1,
            _ => {}
        }
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_tag_end_simple() {
        assert_eq!(find_open_tag_end("<div>x", 0), 5);
    }

    #[test]
    fn open_tag_end_with_attributes() {
        assert_eq!(find_open_tag_end(r#"<div class="a">x"#, 0), 15);
    }

    #[test]
    fn open_tag_end_self_closing() {
        assert_eq!(find_open_tag_end("<br/>x", 0), 5);
    }

    #[test]
    fn open_tag_end_with_gt_in_quotes() {
        assert_eq!(find_open_tag_end(r#"<div class="a>b">x"#, 0), 17);
    }

    #[test]
    fn open_tag_end_with_gt_in_braces() {
        assert_eq!(find_open_tag_end(r#"<button on:click={x > 5}>"#, 0), 25);
    }
}
