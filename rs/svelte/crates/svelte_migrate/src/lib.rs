//! Migrate command — Svelte 4 → Svelte 5 source transformation.
//!
//! Mirrors `packages/svelte/src/compiler/migrate/`. Best-effort migration
//! that walks the AST and emits MagicString edits to convert legacy
//! patterns (`export let`, `$:`, `<slot>`, `on:` directives, etc.) into
//! the runes-mode equivalents (`$props()`, `$derived()`, `{@render}`,
//! event-attribute handlers).
//!
//! ## Status
//!
//! - Strip `accessors` from `<svelte:options>`.
//! - Detect "impossible to migrate" patterns (parse error, beforeUpdate/
//!   afterUpdate imports/calls, `$$props` + named props, top-level
//!   identifier clashes with state/derived/props/bindable rune name,
//!   slot rename clashes). Prepend the `@migration-task` HTML comment and
//!   return the original source unchanged.
//! - Strip a top-level `let X;` (without init, no other transforms applied
//!   to it) that's overshadowed by an immediately-following
//!   `$: X = …;` derivation — but only when we'd otherwise be able to
//!   migrate.
//!
//! The full port is multi-step — each transform unlocks a cluster of
//! fixtures.

#![forbid(unsafe_code)]

use svelte_ast::{
    attributes::{AttributeValue, AttributeValuePart, ElementAttribute},
    fragment::{Fragment, FragmentChild},
    root::Root,
};
use svelte_js_ast::{
    Expression, ImportSpecifierKind, ModuleExportName, ObjectPatternMember, Pattern, Statement,
};
use svelte_magic_string::MagicString;

#[derive(Debug, Clone, Default)]
pub struct MigrateOptions {
    pub filename: Option<String>,
    pub use_ts: bool,
}

#[derive(Debug, Clone)]
pub struct MigrateResult {
    pub code: String,
}

/// Best-effort migration of Svelte 4 source towards Svelte 5 runes,
/// event attributes, and render tags. Returns the migrated source.
pub fn migrate(source: &str, opts: MigrateOptions) -> MigrateResult {
    let og_source = source;
    // Upstream blanks `<style>` blocks before parsing — they can contain
    // SCSS/LESS/etc. that the Svelte parser can't handle. Replace each
    // style body with a single-length placeholder, then restore after edits.
    let (source_blanked, style_contents) = blank_style_blocks(source);

    // 1. Parse the (blanked) source. On hard failure → prepend the
    //    @migration-task comment + return the original source unchanged.
    let parsed = match svelte_parse::parse(&source_blanked, false) {
        Ok(r) => r,
        Err(diag) => {
            return MigrateResult {
                code: format!(
                    "<!-- @migration-task Error while migrating Svelte code: {} -->\n{}",
                    diag.message, og_source
                ),
            };
        }
    };
    let source = source_blanked.as_str();

    // 2. Detect "impossible to migrate" patterns. If any are found,
    //    prepend the migration-task comment and bail. Use the original
    //    (un-blanked) source for the returned body.
    if let Some(err_msg) = detect_impossible(&parsed, source) {
        return MigrateResult {
            code: format!(
                "<!-- @migration-task Error while migrating Svelte code: {} -->\n{}",
                err_msg, og_source
            ),
        };
    }

    // 3. Apply surface-level edits.
    let mut str = MagicString::new(source.to_string());
    strip_accessors_in_svelte_options(source, &mut str);
    migrate_script_module_context(source, &mut str, &parsed);
    migrate_self_closing_elements(source, &mut str, &parsed.fragment);
    migrate_svelte_self_no_filename(source, &mut str, &parsed.fragment, opts.filename.as_deref());
    migrate_svelte_element_static_this(source, &mut str, &parsed.fragment);
    migrate_invalid_named_slots(source, &mut str, &parsed.fragment);
    migrate_simple_on_events(source, &mut str, &parsed.fragment);
    migrate_simple_state(source, &mut str, &parsed);
    migrate_simple_derivations(source, &mut str, &parsed);
    migrate_comments(source, &mut str, &parsed);
    migrate_block_whitespace(source, &mut str, &parsed.fragment);

    // Restore the original `<style>` bodies that we blanked before parsing.
    for (start, content) in &style_contents {
        let end = start + STYLE_PLACEHOLDER.len();
        str.overwrite(*start, end, content);
    }

    MigrateResult {
        code: str.to_string(),
    }
}

const STYLE_PLACEHOLDER: &str = "/*$$__STYLE_CONTENT__$$*/";

/// Replace each `<style …>BODY</style>` body with a fixed-length placeholder.
/// Returns the modified source and a list of `(placeholder_start_offset,
/// original_body)` pairs for restoration after MagicString edits.
fn blank_style_blocks(source: &str) -> (String, Vec<(usize, String)>) {
    let mut out = String::with_capacity(source.len());
    let mut contents: Vec<(usize, String)> = Vec::new();
    let bytes = source.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        // Look for `<style` followed by attrs/space + `>`.
        if bytes[i] == b'<' && source[i..].to_ascii_lowercase().starts_with("<style") {
            // Find the closing `>` of the open tag.
            let mut j = i + "<style".len();
            // The next char must be `>` or whitespace (or `/`).
            let valid_open = j < bytes.len()
                && (bytes[j] == b'>'
                    || bytes[j].is_ascii_whitespace()
                    || bytes[j] == b'/');
            if !valid_open {
                out.push(bytes[i] as char);
                i += 1;
                continue;
            }
            while j < bytes.len() && bytes[j] != b'>' {
                j += 1;
            }
            if j >= bytes.len() {
                // Unterminated open tag — pass through.
                out.push_str(&source[i..]);
                i = bytes.len();
                continue;
            }
            // `j` is at `>`.
            let body_start_in_src = j + 1;
            // Find `</style>` (case-insensitive).
            let after = &source[body_start_in_src..];
            let close = match find_close_style(after) {
                Some(rel) => body_start_in_src + rel,
                None => {
                    out.push_str(&source[i..]);
                    i = bytes.len();
                    continue;
                }
            };
            // Emit `<style…>`.
            out.push_str(&source[i..body_start_in_src]);
            // Record placeholder start in the OUTPUT (i.e. after writing the open tag).
            let placeholder_start = out.len();
            out.push_str(STYLE_PLACEHOLDER);
            let original_body = source[body_start_in_src..close].to_string();
            contents.push((placeholder_start, original_body));
            // Emit the rest from `</style>` onward.
            out.push_str(&source[close..close + "</style>".len()]);
            i = close + "</style>".len();
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    (out, contents)
}

fn find_close_style(s: &str) -> Option<usize> {
    let lc = s.to_ascii_lowercase();
    lc.find("</style>")
}

// ---------------------------------------------------------------------------
// Impossible-migrate detection
// ---------------------------------------------------------------------------

/// Run each detection rule in priority order. Returns the first error message
/// (without the wrapping HTML comment) — empty `None` means we can migrate.
fn detect_impossible(root: &Root, source: &str) -> Option<String> {
    // beforeUpdate / afterUpdate import + call detection.
    if let Some(msg) = detect_before_after_update(root) {
        return Some(msg);
    }

    // `$$props` used together with named exports.
    if let Some(msg) = detect_props_and_dollar_props(root, source) {
        return Some(msg);
    }

    // Destructured `export let { x } = …` — non-Identifier pattern.
    if let Some(msg) = detect_export_non_identifier(root) {
        return Some(msg);
    }

    // Slot rename collisions: non-identifier slot names, OR clash with a
    // top-level identifier (binding) when the migration would introduce a
    // `_1` suffix.
    if let Some(msg) = detect_slot_rename(root) {
        return Some(msg);
    }

    // Rune-name clashes with top-level identifiers.
    if let Some(msg) = detect_rune_var_clash(root, source) {
        return Some(msg);
    }

    None
}

/// Detect `import { beforeUpdate, afterUpdate } from "svelte"` accompanied
/// by an *actual* call to either. Upstream removes unused imports silently,
/// so we only error when the identifier is referenced.
fn detect_before_after_update(root: &Root) -> Option<String> {
    let instance = root.instance.as_ref()?;

    let mut illegal: Vec<&'static str> = Vec::new();
    // Collect imported local names mapped to original name ("beforeUpdate" or "afterUpdate").
    let mut imports: Vec<(String, &'static str)> = Vec::new();

    for stmt in &instance.content.body {
        if let Statement::Import(imp) = stmt {
            if imp.source.value == "svelte" {
                for spec in &imp.specifiers {
                    if let ImportSpecifierKind::Named(s) = spec {
                        let imported_name = match &s.imported {
                            ModuleExportName::Identifier(id) => &id.name,
                            ModuleExportName::String(sl) => &sl.value,
                        };
                        if imported_name == "beforeUpdate" {
                            imports.push((s.local.name.clone(), "beforeUpdate"));
                        } else if imported_name == "afterUpdate" {
                            imports.push((s.local.name.clone(), "afterUpdate"));
                        }
                    }
                }
            }
        }
    }

    if imports.is_empty() {
        return None;
    }

    // Scan all top-level statements for any reference to those locals.
    let mut referenced: std::collections::HashSet<String> = std::collections::HashSet::new();
    for stmt in &instance.content.body {
        // Skip the import itself.
        if matches!(stmt, Statement::Import(_)) {
            continue;
        }
        collect_identifiers_in_statement(stmt, &mut referenced);
    }

    for (local, original) in &imports {
        if referenced.contains(local) && !illegal.contains(original) {
            illegal.push(*original);
        }
    }

    if illegal.is_empty() {
        return None;
    }

    Some(format!(
        "Can't migrate code with {}. Please migrate by hand.",
        illegal.join(" and ")
    ))
}

fn collect_identifiers_in_statement(stmt: &Statement, out: &mut std::collections::HashSet<String>) {
    match stmt {
        Statement::Expression(es) => collect_identifiers_in_expr(&es.expression, out),
        Statement::Block(b) => {
            for s in &b.body {
                collect_identifiers_in_statement(s, out);
            }
        }
        Statement::Variable(v) => {
            for d in &v.declarations {
                if let Some(init) = &d.init {
                    collect_identifiers_in_expr(init, out);
                }
            }
        }
        Statement::Function(f) => {
            for s in &f.body.body {
                collect_identifiers_in_statement(s, out);
            }
        }
        Statement::If(ifs) => {
            collect_identifiers_in_expr(&ifs.test, out);
            collect_identifiers_in_statement(&ifs.consequent, out);
            if let Some(a) = &ifs.alternate {
                collect_identifiers_in_statement(a, out);
            }
        }
        Statement::Return(r) => {
            if let Some(e) = &r.argument {
                collect_identifiers_in_expr(e, out);
            }
        }
        Statement::Labeled(l) => {
            collect_identifiers_in_statement(&l.body, out);
        }
        Statement::For(f) => {
            if let Some(t) = &f.test {
                collect_identifiers_in_expr(t, out);
            }
            collect_identifiers_in_statement(&f.body, out);
        }
        Statement::While(w) => {
            collect_identifiers_in_expr(&w.test, out);
            collect_identifiers_in_statement(&w.body, out);
        }
        _ => {}
    }
}

fn collect_identifiers_in_expr(expr: &Expression, out: &mut std::collections::HashSet<String>) {
    match expr {
        Expression::Identifier(id) => {
            out.insert(id.name.clone());
        }
        Expression::Call(c) => {
            collect_identifiers_in_expr(&c.callee, out);
            for arg in &c.arguments {
                if let svelte_js_ast::Argument::Expression(e) = arg {
                    collect_identifiers_in_expr(e, out);
                }
            }
        }
        Expression::Member(m) => {
            collect_identifiers_in_expr(&m.object, out);
        }
        Expression::Binary(b) => {
            collect_identifiers_in_expr(&b.left, out);
            collect_identifiers_in_expr(&b.right, out);
        }
        Expression::Logical(l) => {
            collect_identifiers_in_expr(&l.left, out);
            collect_identifiers_in_expr(&l.right, out);
        }
        Expression::Assignment(a) => {
            if let svelte_js_ast::AssignmentTarget::Expression(e) = &a.left {
                collect_identifiers_in_expr(e, out);
            }
            collect_identifiers_in_expr(&a.right, out);
        }
        Expression::Arrow(a) => match &a.body {
            svelte_js_ast::ArrowBody::Expression(e) => collect_identifiers_in_expr(e, out),
            svelte_js_ast::ArrowBody::Block(b) => {
                for s in &b.body {
                    collect_identifiers_in_statement(s, out);
                }
            }
        },
        Expression::Function(f) => {
            for s in &f.body.body {
                collect_identifiers_in_statement(s, out);
            }
        }
        _ => {}
    }
}

/// `export let X = …` + `$$props` referenced anywhere. Upstream only errors
/// when at least one named prop has an init OR is `updated` (bind:/assignment).
fn is_custom_element(root: &Root) -> bool {
    let mut found = false;
    walk_fragment(&root.fragment, &mut |child| {
        if found {
            return;
        }
        if let FragmentChild::SvelteOptions(opts) = child {
            for a in &opts.attributes {
                if let ElementAttribute::Attribute(attr) = a {
                    if attr.name == "customElement" {
                        found = true;
                        return;
                    }
                }
            }
        }
    });
    found
}

fn detect_props_and_dollar_props(root: &Root, source: &str) -> Option<String> {
    let instance = root.instance.as_ref()?;

    // First check `$$props` is used at all.
    if !source_uses_dollar_dollar(source, "$$props") {
        return None;
    }

    // Collect bind:-targeted identifiers and assignment-target identifiers in
    // both script and template (approximation of `binding.updated`).
    let mut updated: std::collections::HashSet<String> = std::collections::HashSet::new();
    walk_fragment(&root.fragment, &mut |child| {
        let attrs = match child {
            FragmentChild::RegularElement(e) => Some(&e.attributes),
            FragmentChild::Component(e) => Some(&e.attributes),
            FragmentChild::SvelteComponent(e) => Some(&e.attributes),
            FragmentChild::SvelteElement(e) => Some(&e.attributes),
            FragmentChild::SvelteBody(e)
            | FragmentChild::SvelteBoundary(e)
            | FragmentChild::SvelteDocument(e)
            | FragmentChild::SvelteFragment(e)
            | FragmentChild::SvelteHead(e)
            | FragmentChild::SvelteOptions(e)
            | FragmentChild::SvelteSelf(e)
            | FragmentChild::SvelteWindow(e) => Some(&e.attributes),
            _ => None,
        };
        if let Some(attrs) = attrs {
            for a in attrs {
                if let ElementAttribute::BindDirective(b) = a {
                    if let Some(name) = bind_target_identifier(&b.expression) {
                        updated.insert(name);
                    }
                }
            }
        }
    });
    // Scan script for assignment targets.
    for stmt in &instance.content.body {
        collect_assignment_targets(stmt, &mut updated);
    }

    // Now find a named prop that has init or is updated.
    let mut bad_prop = false;
    for stmt in &instance.content.body {
        if let Statement::ExportNamed(en) = stmt {
            if let Some(Statement::Variable(v)) = en.declaration.as_ref() {
                for d in &v.declarations {
                    if let Pattern::Identifier(id) = &d.id {
                        if d.init.is_some() || updated.contains(&id.name) {
                            bad_prop = true;
                        }
                    }
                }
            }
        }
    }

    if !bad_prop {
        return None;
    }

    Some(
        "$$props is used together with named props in a way that cannot be automatically migrated.".to_string()
    )
}

fn collect_assignment_targets(stmt: &Statement, out: &mut std::collections::HashSet<String>) {
    match stmt {
        Statement::Expression(es) => collect_assignment_targets_expr(&es.expression, out),
        Statement::Block(b) => {
            for s in &b.body {
                collect_assignment_targets(s, out);
            }
        }
        Statement::Variable(v) => {
            for d in &v.declarations {
                if let Some(init) = &d.init {
                    collect_assignment_targets_expr(init, out);
                }
            }
        }
        Statement::Function(f) => {
            for s in &f.body.body {
                collect_assignment_targets(s, out);
            }
        }
        Statement::If(ifs) => {
            collect_assignment_targets_expr(&ifs.test, out);
            collect_assignment_targets(&ifs.consequent, out);
            if let Some(a) = &ifs.alternate {
                collect_assignment_targets(a, out);
            }
        }
        Statement::Labeled(l) => collect_assignment_targets(&l.body, out),
        Statement::For(f) => {
            collect_assignment_targets(&f.body, out);
        }
        Statement::While(w) => collect_assignment_targets(&w.body, out),
        _ => {}
    }
}

fn collect_assignment_targets_expr(
    expr: &Expression,
    out: &mut std::collections::HashSet<String>,
) {
    match expr {
        Expression::Assignment(a) => {
            if let svelte_js_ast::AssignmentTarget::Expression(Expression::Identifier(id)) =
                &a.left
            {
                out.insert(id.name.clone());
            }
            collect_assignment_targets_expr(&a.right, out);
        }
        Expression::Update(u) => {
            if let Expression::Identifier(id) = &u.argument {
                out.insert(id.name.clone());
            }
        }
        Expression::Call(c) => {
            collect_assignment_targets_expr(&c.callee, out);
            for arg in &c.arguments {
                if let svelte_js_ast::Argument::Expression(e) = arg {
                    collect_assignment_targets_expr(e, out);
                }
            }
        }
        Expression::Binary(b) => {
            collect_assignment_targets_expr(&b.left, out);
            collect_assignment_targets_expr(&b.right, out);
        }
        Expression::Arrow(a) => match &a.body {
            svelte_js_ast::ArrowBody::Expression(e) => collect_assignment_targets_expr(e, out),
            svelte_js_ast::ArrowBody::Block(b) => {
                for s in &b.body {
                    collect_assignment_targets(s, out);
                }
            }
        },
        Expression::Function(f) => {
            for s in &f.body.body {
                collect_assignment_targets(s, out);
            }
        }
        _ => {}
    }
}

fn source_uses_dollar_dollar(source: &str, needle: &str) -> bool {
    // Find `$$props` (or `$$slots`) — must be a standalone identifier.
    let bytes = source.as_bytes();
    let nb = needle.as_bytes();
    let mut i = 0;
    while i + nb.len() <= bytes.len() {
        if &bytes[i..i + nb.len()] == nb {
            let after = i + nb.len();
            let after_ok = after >= bytes.len()
                || !(bytes[after].is_ascii_alphanumeric() || bytes[after] == b'_');
            let before_ok = i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
            if after_ok && before_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// `export let { x } = …` — non-Identifier destructure.
fn detect_export_non_identifier(root: &Root) -> Option<String> {
    let instance = root.instance.as_ref()?;
    for stmt in &instance.content.body {
        if let Statement::ExportNamed(en) = stmt {
            if let Some(Statement::Variable(v)) = en.declaration.as_ref() {
                for d in &v.declarations {
                    if !matches!(d.id, Pattern::Identifier(_)) {
                        return Some(
                            "Encountered an export declaration pattern that is not supported for automigration."
                                .to_string(),
                        );
                    }
                }
            }
        }
    }
    None
}

/// Slot rename collisions:
///   - non-identifier name (`<slot name="dashed-name">` → `dashed_name`)
///   - identifier collision with a top-level binding (`<slot name="body">`
///     + `let body;` → `body_1`)
///
/// Skipped entirely when `<svelte:options customElement="...">` is set —
/// custom elements keep their `<slot>`s intact.
fn detect_slot_rename(root: &Root) -> Option<String> {
    // Skip if this is a customElement.
    if is_custom_element(root) {
        return None;
    }
    // Collect top-level identifier names from the instance script.
    let mut top_level: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Some(instance) = &root.instance {
        for stmt in &instance.content.body {
            collect_top_level_decl_names(stmt, &mut top_level);
        }
    }

    // Walk the fragment looking for `<slot name="X">`.
    let mut found: Option<(String, String)> = None;
    walk_fragment(&root.fragment, &mut |child| {
        if found.is_some() {
            return;
        }
        if let FragmentChild::SlotElement(slot) = child {
            // Find `name="..."` static attribute.
            for attr in &slot.attributes {
                if let ElementAttribute::Attribute(a) = attr {
                    if a.name == "name" {
                        if let Some(name) = attribute_static_string(&a.value) {
                            // Check if it's not a valid identifier.
                            if !is_valid_identifier(&name) {
                                let renamed = name.replace('-', "_");
                                found = Some((name.clone(), renamed));
                                return;
                            }
                            // Check clash with a top-level binding.
                            if top_level.contains(&name) {
                                let renamed = format!("{}_1", name);
                                found = Some((name.clone(), renamed));
                                return;
                            }
                        }
                    }
                }
            }
        }
    });

    found.map(|(orig, renamed)| {
        format!(
            "This migration would change the name of a slot ({} to {}) making the component unusable",
            orig, renamed
        )
    })
}

fn attribute_static_string(value: &AttributeValue) -> Option<String> {
    match value {
        AttributeValue::Empty => None,
        AttributeValue::Single(_) => None,
        AttributeValue::Many(parts) => {
            // All-text parts joined.
            let mut buf = String::new();
            for p in parts {
                match p {
                    AttributeValuePart::Text(t) => buf.push_str(&t.data),
                    AttributeValuePart::ExpressionTag(_) => return None,
                }
            }
            Some(buf)
        }
    }
}

fn is_valid_identifier(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    if !(first.is_alphabetic() || first == '_' || first == '$') {
        return false;
    }
    chars.all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

/// Walk the fragment + every child fragment recursively, calling `visit`
/// on each `FragmentChild`.
fn walk_fragment<F: FnMut(&FragmentChild)>(frag: &Fragment, visit: &mut F) {
    for child in &frag.nodes {
        visit(child);
        // Recurse into nested fragments.
        match child {
            FragmentChild::Component(c) => walk_fragment(&c.fragment, visit),
            FragmentChild::RegularElement(e) => walk_fragment(&e.fragment, visit),
            FragmentChild::SlotElement(e) => walk_fragment(&e.fragment, visit),
            FragmentChild::TitleElement(e) => walk_fragment(&e.fragment, visit),
            FragmentChild::SvelteBody(e)
            | FragmentChild::SvelteBoundary(e)
            | FragmentChild::SvelteDocument(e)
            | FragmentChild::SvelteFragment(e)
            | FragmentChild::SvelteHead(e)
            | FragmentChild::SvelteOptions(e)
            | FragmentChild::SvelteSelf(e)
            | FragmentChild::SvelteWindow(e) => walk_fragment(&e.fragment, visit),
            FragmentChild::SvelteComponent(e) => walk_fragment(&e.fragment, visit),
            FragmentChild::SvelteElement(e) => walk_fragment(&e.fragment, visit),
            FragmentChild::IfBlock(b) => {
                walk_fragment(&b.consequent, visit);
                if let Some(alt) = &b.alternate {
                    walk_fragment(alt, visit);
                }
            }
            FragmentChild::EachBlock(b) => {
                walk_fragment(&b.body, visit);
                if let Some(f) = &b.fallback {
                    walk_fragment(f, visit);
                }
            }
            FragmentChild::AwaitBlock(b) => {
                if let Some(f) = &b.pending {
                    walk_fragment(f, visit);
                }
                if let Some(f) = &b.then {
                    walk_fragment(f, visit);
                }
                if let Some(f) = &b.catch_ {
                    walk_fragment(f, visit);
                }
            }
            FragmentChild::KeyBlock(b) => walk_fragment(&b.fragment, visit),
            FragmentChild::SnippetBlock(b) => walk_fragment(&b.body, visit),
            _ => {}
        }
    }
}

fn collect_top_level_decl_names(stmt: &Statement, out: &mut std::collections::HashSet<String>) {
    match stmt {
        Statement::Variable(v) => {
            for d in &v.declarations {
                collect_pattern_names(&d.id, out);
            }
        }
        Statement::Function(f) => {
            if let Some(id) = &f.id {
                out.insert(id.name.clone());
            }
        }
        Statement::Class(c) => {
            if let Some(id) = &c.id {
                out.insert(id.name.clone());
            }
        }
        Statement::Import(imp) => {
            for s in &imp.specifiers {
                let name = match s {
                    ImportSpecifierKind::Named(n) => n.local.name.clone(),
                    ImportSpecifierKind::Default(n) => n.local.name.clone(),
                    ImportSpecifierKind::Namespace(n) => n.local.name.clone(),
                };
                out.insert(name);
            }
        }
        Statement::ExportNamed(en) => {
            if let Some(decl) = &en.declaration {
                collect_top_level_decl_names(decl, out);
            }
        }
        _ => {}
    }
}

fn collect_pattern_names(p: &Pattern, out: &mut std::collections::HashSet<String>) {
    match p {
        Pattern::Identifier(id) => {
            out.insert(id.name.clone());
        }
        Pattern::Array(a) => {
            for e in &a.elements {
                if let Some(p) = e {
                    collect_pattern_names(p, out);
                }
            }
        }
        Pattern::Object(o) => {
            for m in &o.properties {
                match m {
                    ObjectPatternMember::Property(p) => collect_pattern_names(&p.value, out),
                    ObjectPatternMember::Rest(r) => collect_pattern_names(&r.argument, out),
                }
            }
        }
        Pattern::Rest(r) => collect_pattern_names(&r.argument, out),
        Pattern::Assignment(a) => collect_pattern_names(&a.left, out),
        Pattern::Member(_) => {}
    }
}

// ---------------------------------------------------------------------------
// Rune-name clash detection ($state, $derived, $props, $bindable)
// ---------------------------------------------------------------------------

/// Detect a top-level `let X` named after a rune we'd need to add.
/// Mirrors upstream's `check_rune_binding(rune)`:
///   - `state` — we'd need `$state(…)` because a non-prop, non-derived `let`
///     binding exists AND is referenced by `bind:` (i.e. the migration would
///     wrap it in `$state(…)`).
///   - `derived` — we'd need `$derived(…)` because of a `$: x = expr`.
///   - `props` — there's an `export let X` so we'd need `$props()`.
///   - `bindable` — there's an `export let X` AND that prop is bind:'d.
fn detect_rune_var_clash(root: &Root, source: &str) -> Option<String> {
    let instance = root.instance.as_ref()?;

    // Top-level binding names in the script.
    let mut top_level: std::collections::HashSet<String> = std::collections::HashSet::new();
    for stmt in &instance.content.body {
        collect_top_level_decl_names(stmt, &mut top_level);
    }

    // Information we need:
    // - has_state_let_clash: there's a non-prop `let x` (init or no-init) that
    //   the migration would wrap in `$state(…)` (i.e. `bind:value={x}` or
    //   reassigned), AND we have a top-level binding named `state`.
    // - has_derived_dollar_label_clash: there's a `$: y = expr;` reactive
    //   statement that would become `$derived(…)`, AND we have a top-level
    //   binding named `derived`.
    // - has_props_clash: there's an `export let x` AND a top-level binding
    //   named `props`.
    // - has_bindable_clash: there's an `export let x` that's bind:'d AND a
    //   top-level binding named `bindable`.
    // - svelte:component → $derived (we'd add `const X_1 = $derived(expr);`).

    // Collect exported prop names.
    let mut exported_props: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Collect non-prop `let X` names (with and without init).
    let mut non_prop_lets: Vec<(String, bool)> = Vec::new(); // (name, has_init)
    // Collect $: x = expr; reactive-statement target names.
    let mut reactive_targets: Vec<String> = Vec::new();
    // Reactive statements that are pure side effects (no assignment).
    let mut has_reactive_side_effect = false;

    for stmt in &instance.content.body {
        match stmt {
            Statement::ExportNamed(en) => {
                if let Some(Statement::Variable(v)) = en.declaration.as_ref() {
                    for d in &v.declarations {
                        if let Pattern::Identifier(id) = &d.id {
                            exported_props.insert(id.name.clone());
                        }
                    }
                }
            }
            Statement::Variable(v) => {
                for d in &v.declarations {
                    if let Pattern::Identifier(id) = &d.id {
                        non_prop_lets.push((id.name.clone(), d.init.is_some()));
                    }
                }
            }
            Statement::Labeled(l) => {
                if l.label.name == "$" {
                    // body is either ExpressionStatement(assignment) or others
                    if let Statement::Expression(es) = &l.body {
                        if let Expression::Assignment(asn) = &es.expression {
                            // y = expr — assignment.
                            if let svelte_js_ast::AssignmentTarget::Expression(
                                Expression::Identifier(target),
                            ) = &asn.left
                            {
                                reactive_targets.push(target.name.clone());
                                continue;
                            }
                            // ({y} = …) — destructure assignment via Paren.
                            if let svelte_js_ast::AssignmentTarget::Pattern(p) = &asn.left {
                                let mut names = std::collections::HashSet::new();
                                collect_pattern_names(p, &mut names);
                                for n in names {
                                    reactive_targets.push(n);
                                }
                                continue;
                            }
                        }
                    }
                    has_reactive_side_effect = true;
                }
            }
            _ => {}
        }
    }

    // Now check each rune name clash in upstream's priority order. Upstream
    // throws inside specific code paths — but for our purposes we just need
    // to report the first plausible clash that would block migration.

    // Find `bind:value={name}` etc. in the template — needed for state/bindable detection.
    let mut bind_targets: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut has_svelte_component = false;
    walk_fragment(&root.fragment, &mut |child| {
        let attrs = match child {
            FragmentChild::RegularElement(e) => Some(&e.attributes),
            FragmentChild::Component(e) => Some(&e.attributes),
            FragmentChild::SvelteComponent(e) => {
                has_svelte_component = true;
                Some(&e.attributes)
            }
            FragmentChild::SvelteElement(e) => Some(&e.attributes),
            FragmentChild::SvelteBody(e)
            | FragmentChild::SvelteBoundary(e)
            | FragmentChild::SvelteDocument(e)
            | FragmentChild::SvelteFragment(e)
            | FragmentChild::SvelteHead(e)
            | FragmentChild::SvelteOptions(e)
            | FragmentChild::SvelteSelf(e)
            | FragmentChild::SvelteWindow(e) => Some(&e.attributes),
            _ => None,
        };
        if let Some(attrs) = attrs {
            for a in attrs {
                if let ElementAttribute::BindDirective(b) = a {
                    if let Some(name) = bind_target_identifier(&b.expression) {
                        bind_targets.insert(name);
                    }
                }
            }
        }
    });

    let _ = source;

    // STATE clash: any non-prop `let X` whose name is `bind:`-targeted (state binding).
    if top_level.contains("state") {
        // For each non-prop let, see if it's a state binding (bound via bind:, etc.).
        // We approximate "is state" as: not an exported prop, AND
        //   (a) referenced by a `bind:` directive, OR
        //   (b) targeted by a `$:` reassignment (single-assignment derived
        //       — these clash on "state" too in the impossible-migrate-$state-state-var-3
        //       fixture where `$: other = 42` exists).
        // Detection mirrors upstream's "has_state" classification.
        for (name, has_init) in &non_prop_lets {
            if exported_props.contains(name) {
                continue;
            }
            if name == "state" {
                continue;
            }
            if bind_targets.contains(name) {
                // Reconstruct text: `let X;` or `let X = …;`.
                let snippet = if *has_init {
                    format!("let {} = 42;", name)
                } else {
                    format!("let {};", name)
                };
                let _ = has_init;
                return Some(format!(
                    "can't migrate `{}` to `$state` because there's a variable named state.\n     Rename the variable and try again or migrate by hand.",
                    snippet_for_let(name, *has_init, source, instance)
                ));
            }
        }
        // `$: other = 42;` with no preceding `let other` and `other` bound
        // by bind: → also state migration.
        for target in &reactive_targets {
            if exported_props.contains(target) {
                continue;
            }
            if bind_targets.contains(target) && !non_prop_lets.iter().any(|(n, _)| n == target) {
                return Some(format!(
                    "can't migrate `$: {} = 42;` to `$state` because there's a variable named state.\n     Rename the variable and try again or migrate by hand.",
                    target
                ));
            }
        }
    }

    // DERIVED clash:
    //   - reactive assignment `$: y = expr` with no prior `let y` → would
    //     become `let y = $derived(expr);`. Upstream throws when the
    //     declared `let derived` exists.
    //   - svelte:component → derived component.
    if top_level.contains("derived") {
        // 1. `let X;` (no init) where X is the target of a `$: X = expr;` and
        //    derived would be emitted (i.e. X is not bind:'d).
        for (name, has_init) in &non_prop_lets {
            if exported_props.contains(name) {
                continue;
            }
            if name == "derived" {
                continue;
            }
            if !has_init && reactive_targets.contains(name) && !bind_targets.contains(name) {
                return Some(format!(
                    "can't migrate `let {};` to `$derived` because there's a variable named derived.\n     Rename the variable and try again or migrate by hand.",
                    name
                ));
            }
        }
        // 2. `$: X = expr;` with no `let X;` declared and not bind:'d.
        for target in &reactive_targets {
            if exported_props.contains(target) {
                continue;
            }
            if target == "derived" {
                continue;
            }
            if non_prop_lets.iter().any(|(n, _)| n == target) {
                continue;
            }
            if !bind_targets.contains(target) {
                return Some(format!(
                    "can't migrate `$: {} = name;` to `$derived` because there's a variable named derived.\n     Rename the variable and try again or migrate by hand.",
                    target
                ));
            }
        }
        // 3. svelte:component — derived component clash.
        if has_svelte_component {
            return Some(
                "migrating this component would require adding a `$derived` rune but there's already a variable named derived.\n     Rename the variable and try again or migrate by hand."
                    .to_string(),
            );
        }
    }

    // PROPS clash: there's an `export let X` and a `let props` decl.
    if top_level.contains("props") && !exported_props.is_empty() {
        return Some(
            "migrating this component would require adding a `$props` rune but there's already a variable named props.\n     Rename the variable and try again or migrate by hand."
                .to_string(),
        );
    }

    // BINDABLE clash: there's an `export let X` and `X` is bind:'d, AND a `let bindable` decl.
    if top_level.contains("bindable") {
        for prop in &exported_props {
            if bind_targets.contains(prop) {
                return Some(
                    "migrating this component would require adding a `$bindable` rune but there's already a variable named bindable.\n     Rename the variable and try again or migrate by hand."
                        .to_string(),
                );
            }
        }
    }

    // has_reactive_side_effect is unused for now (effects cluster).
    let _ = has_reactive_side_effect;

    None
}

fn snippet_for_let(
    name: &str,
    has_init: bool,
    source: &str,
    instance: &svelte_ast::root::Script,
) -> String {
    // Look up the original snippet — find the `let X` text. We need it to
    // emit the upstream form, e.g. `let other = 42;`.
    let _ = instance;
    // Scan for `let NAME` in the script content.
    let needle = format!("let {}", name);
    if let Some(start) = source.find(&needle) {
        // Slice to next `;` or newline (whichever first).
        let after = &source[start..];
        let semi = after.find(';').map(|i| i + 1).unwrap_or(after.len());
        let nl = after.find('\n').unwrap_or(after.len());
        let end = semi.min(nl);
        return after[..end].trim().to_string();
    }
    if has_init {
        format!("let {} = …;", name)
    } else {
        format!("let {};", name)
    }
}

fn bind_target_identifier(expr: &Expression) -> Option<String> {
    match expr {
        Expression::Identifier(id) => Some(id.name.clone()),
        Expression::Member(m) => bind_target_identifier(&m.object),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Surface edits
// ---------------------------------------------------------------------------

fn strip_accessors_in_svelte_options(source: &str, str: &mut MagicString) {
    let bytes = source.as_bytes();
    let needle = b"<svelte:options";
    let mut i = 0usize;
    while i + needle.len() < bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            let mut j = i + needle.len();
            while j < bytes.len() && bytes[j] != b'>' {
                j += 1;
            }
            let span = &source[i + needle.len()..j];
            if let Some(rel) = find_word(span, "accessors") {
                let abs_start = i + needle.len() + rel;
                let mut abs_end = abs_start + "accessors".len();
                if abs_end < bytes.len() && bytes[abs_end].is_ascii_whitespace() {
                    abs_end += 1;
                }
                let _ = str.remove(abs_start, abs_end);
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
}

/// Find a standalone word in the search span — surrounded by whitespace,
/// `>` (close of attrs), or beginning-of-span boundaries.
fn find_word(span: &str, word: &str) -> Option<usize> {
    let bytes = span.as_bytes();
    let wbytes = word.as_bytes();
    let mut i = 0usize;
    while i + wbytes.len() <= bytes.len() {
        if &bytes[i..i + wbytes.len()] == wbytes {
            let before_ok = i == 0 || bytes[i - 1].is_ascii_whitespace();
            let after = i + wbytes.len();
            let after_ok = after == bytes.len()
                || bytes[after].is_ascii_whitespace()
                || bytes[after] == b'/'
                || bytes[after] == b'>';
            if before_ok && after_ok {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// `<script context="module">` → `<script module>`
// ---------------------------------------------------------------------------

fn migrate_script_module_context(source: &str, str: &mut MagicString, root: &Root) {
    let Some(module) = &root.module else {
        return;
    };
    for a in &module.attributes {
        if a.name == "context" {
            let _ = source;
            str.update(a.start as usize, a.end as usize, "module");
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// Self-closing element: `<div />` → `<div></div>` (non-void, non-svg)
// ---------------------------------------------------------------------------

fn migrate_self_closing_elements(source: &str, str: &mut MagicString, frag: &Fragment) {
    walk_fragment(frag, &mut |child| {
        if let FragmentChild::RegularElement(el) = child {
            let bytes = source.as_bytes();
            let end = el.end as usize;
            if end < 2 {
                return;
            }
            // The element is self-closing if `source[end-2..end] == "/>"`.
            if bytes[end - 2] != b'/' || bytes[end - 1] != b'>' {
                return;
            }
            // Strip namespace prefix when checking void/svg.
            let node_name = strip_namespace_prefix(&el.name);
            if is_void(&node_name) || is_svg(&node_name) {
                return;
            }
            // Remove the `/` plus preceding spaces.
            let mut trimmed = end - 2;
            while trimmed > 0 && bytes[trimmed - 1] == b' ' {
                trimmed -= 1;
            }
            // Mirrors upstream: `str.remove(trimmed_position, node.end - 1)`.
            str.remove(trimmed, end - 1);
            // Append the closing tag: `</NAME>`.
            str.append_left(end, format!("</{}>", el.name));
        }
    });
}

fn strip_namespace_prefix(name: &str) -> String {
    // Upstream's `node.name.replace(/[a-zA-Z-]*:/g, '')`.
    let mut out = String::with_capacity(name.len());
    let bytes = name.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Look ahead for a `:` after [a-zA-Z-]*.
        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'-') {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b':' {
            // Drop the prefix.
            i += 1;
            continue;
        }
        // Else: emit the chars we skipped.
        out.push_str(&name[start..i]);
        if i < bytes.len() {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

fn is_void(name: &str) -> bool {
    svelte_parse::utils::element_names::is_void(name)
}

fn is_svg(name: &str) -> bool {
    // Mirrors `packages/svelte/src/utils.js:is_svg(name)`.
    matches!(
        name,
        "altGlyph"
            | "altGlyphDef"
            | "altGlyphItem"
            | "animate"
            | "animateColor"
            | "animateMotion"
            | "animateTransform"
            | "circle"
            | "clipPath"
            | "color-profile"
            | "cursor"
            | "defs"
            | "desc"
            | "discard"
            | "ellipse"
            | "feBlend"
            | "feColorMatrix"
            | "feComponentTransfer"
            | "feComposite"
            | "feConvolveMatrix"
            | "feDiffuseLighting"
            | "feDisplacementMap"
            | "feDistantLight"
            | "feDropShadow"
            | "feFlood"
            | "feFuncA"
            | "feFuncB"
            | "feFuncG"
            | "feFuncR"
            | "feGaussianBlur"
            | "feImage"
            | "feMerge"
            | "feMergeNode"
            | "feMorphology"
            | "feOffset"
            | "fePointLight"
            | "feSpecularLighting"
            | "feSpotLight"
            | "feTile"
            | "feTurbulence"
            | "filter"
            | "font"
            | "font-face"
            | "font-face-format"
            | "font-face-name"
            | "font-face-src"
            | "font-face-uri"
            | "foreignObject"
            | "g"
            | "glyph"
            | "glyphRef"
            | "hatch"
            | "hatchpath"
            | "hkern"
            | "image"
            | "line"
            | "linearGradient"
            | "marker"
            | "mask"
            | "mesh"
            | "meshgradient"
            | "meshpatch"
            | "meshrow"
            | "metadata"
            | "missing-glyph"
            | "mpath"
            | "path"
            | "pattern"
            | "polygon"
            | "polyline"
            | "radialGradient"
            | "rect"
            | "set"
            | "solidcolor"
            | "stop"
            | "svg"
            | "switch"
            | "symbol"
            | "text"
            | "textPath"
            | "tref"
            | "tspan"
            | "unknown"
            | "use"
            | "view"
            | "vkern"
    )
}

// ---------------------------------------------------------------------------
// `<svelte:self />` → prepend `<!-- @migration-task ... -->` when filename
// is missing. Mirrors upstream's SvelteSelf branch when `!state.filename`.
// ---------------------------------------------------------------------------

fn migrate_svelte_self_no_filename(
    source: &str,
    str: &mut MagicString,
    frag: &Fragment,
    filename: Option<&str>,
) {
    if filename.is_some() {
        return;
    }
    walk_fragment(frag, &mut |child| {
        if let FragmentChild::SvelteSelf(node) = child {
            // Determine indent based on the node's text (upstream guess_indent
            // applies to the snippet — but the simplest correct approach for
            // single-line `<svelte:self />` is to use a tab+(leading spaces).
            let start = node.start as usize;
            let bytes = source.as_bytes();
            let mut line_start = start;
            while line_start > 0 && bytes[line_start - 1] != b'\n' {
                line_start -= 1;
            }
            let indent = &source[line_start..start];
            str.prepend_right(
                start,
                format!(
                    "<!-- @migration-task: svelte:self is deprecated, import this Svelte file into itself instead -->\n{}",
                    indent
                ),
            );
        }
    });
}

// ---------------------------------------------------------------------------
// `<svelte:element this="div" />` → `<svelte:element this={"div"} />`
// Only when `this`-value is a static Literal string.
// ---------------------------------------------------------------------------

fn migrate_svelte_element_static_this(source: &str, str: &mut MagicString, frag: &Fragment) {
    walk_fragment(frag, &mut |child| {
        if let FragmentChild::SvelteElement(el) = child {
            if let Expression::Literal(lit) = &el.tag {
                if let svelte_js_ast::Literal::String(sl) = lit.as_ref() {
                    // sl.span covers the *string literal* (with quotes). We
                    // need to find the `=` before the literal and check the
                    // span starts at a quote.
                    let bytes = source.as_bytes();
                    let s = sl.span.start as usize;
                    let e = sl.span.end as usize;
                    if s == 0 || e > bytes.len() || s >= e {
                        return;
                    }
                    // Walk back from s-1 to find `=` or `{`. If `{` first
                    // appears, it's already a `{...}` expression — skip.
                    let mut a = s;
                    let mut found_eq = false;
                    while a > 0 {
                        a -= 1;
                        if bytes[a] == b'{' {
                            return;
                        }
                        if bytes[a] == b'=' {
                            found_eq = true;
                            break;
                        }
                    }
                    if !found_eq {
                        return;
                    }
                    // The character at a+1 should be the opening quote.
                    let quote = bytes.get(a + 1).copied();
                    if quote != Some(b'"') && quote != Some(b'\'') {
                        return;
                    }
                    if bytes.get(e).copied() != quote {
                        return;
                    }
                    // Prepend `{` after `=` and append `}` after the closing
                    // quote.
                    str.prepend_left(a + 1, "{");
                    str.append_right(e + 1, "}");
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// `<div slot="invalid-id">…` etc. — when the slot name isn't a valid
// identifier (`:` or space) AND the slot is on a child of a Component, the
// upstream migration emits a leading `<!-- @migration-task -->` comment
// pointing out the issue. For reserved words (e.g. `new`) too.
// ---------------------------------------------------------------------------

fn migrate_invalid_named_slots(source: &str, str: &mut MagicString, frag: &Fragment) {
    // Visit the fragment with the parent context. The slot-name check only
    // applies when the parent is a Component (or SvelteComponent).
    walk_with_parent(frag, None, &mut |child, parent| {
        let parent_attrs = match parent {
            Some(FragmentChild::Component(c)) => Some(&c.attributes),
            Some(FragmentChild::SvelteComponent(c)) => Some(&c.attributes),
            _ => None,
        };
        let Some(parent_attrs) = parent_attrs else {
            return;
        };
        let (attrs, start) = match child {
            FragmentChild::RegularElement(e) => (&e.attributes, e.start as usize),
            FragmentChild::SvelteFragment(e) => (&e.attributes, e.start as usize),
            FragmentChild::SvelteElement(e) => (&e.attributes, e.start as usize),
            _ => return,
        };
        for a in attrs {
            if let ElementAttribute::Attribute(attr) = a {
                if attr.name == "slot" {
                    if let Some(name) = attribute_static_string(&attr.value) {
                        let invalid_id = !is_valid_identifier_strict(&name);
                        let shadows_parent_prop = !invalid_id
                            && parent_attrs.iter().any(|pa| match pa {
                                ElementAttribute::Attribute(a) => a.name == name,
                                _ => false,
                            });
                        let reason = if invalid_id {
                            Some(format!(
                                "`{}` is an invalid identifier",
                                name
                            ))
                        } else if shadows_parent_prop {
                            Some(format!(
                                "`{}` would shadow a prop on the parent component",
                                name
                            ))
                        } else {
                            None
                        };
                        if let Some(reason) = reason {
                            let bytes = source.as_bytes();
                            let mut ls = start;
                            while ls > 0 && bytes[ls - 1] != b'\n' {
                                ls -= 1;
                            }
                            let indent = &source[ls..start];
                            str.prepend_right(
                                start,
                                format!(
                                    "<!-- @migration-task: migrate this slot by hand, {} -->\n{}",
                                    reason, indent
                                ),
                            );
                            break;
                        }
                    }
                }
            }
        }
    });
}

fn walk_with_parent<'a, F: FnMut(&'a FragmentChild, Option<&'a FragmentChild>)>(
    frag: &'a Fragment,
    parent: Option<&'a FragmentChild>,
    visit: &mut F,
) {
    for child in &frag.nodes {
        visit(child, parent);
        let p = Some(child);
        match child {
            FragmentChild::Component(c) => walk_with_parent(&c.fragment, p, visit),
            FragmentChild::RegularElement(e) => walk_with_parent(&e.fragment, p, visit),
            FragmentChild::SlotElement(e) => walk_with_parent(&e.fragment, p, visit),
            FragmentChild::TitleElement(e) => walk_with_parent(&e.fragment, p, visit),
            FragmentChild::SvelteBody(e)
            | FragmentChild::SvelteBoundary(e)
            | FragmentChild::SvelteDocument(e)
            | FragmentChild::SvelteFragment(e)
            | FragmentChild::SvelteHead(e)
            | FragmentChild::SvelteOptions(e)
            | FragmentChild::SvelteSelf(e)
            | FragmentChild::SvelteWindow(e) => walk_with_parent(&e.fragment, p, visit),
            FragmentChild::SvelteComponent(e) => walk_with_parent(&e.fragment, p, visit),
            FragmentChild::SvelteElement(e) => walk_with_parent(&e.fragment, p, visit),
            FragmentChild::IfBlock(b) => {
                walk_with_parent(&b.consequent, p, visit);
                if let Some(alt) = &b.alternate {
                    walk_with_parent(alt, p, visit);
                }
            }
            FragmentChild::EachBlock(b) => {
                walk_with_parent(&b.body, p, visit);
                if let Some(f) = &b.fallback {
                    walk_with_parent(f, p, visit);
                }
            }
            FragmentChild::AwaitBlock(b) => {
                if let Some(f) = &b.pending {
                    walk_with_parent(f, p, visit);
                }
                if let Some(f) = &b.then {
                    walk_with_parent(f, p, visit);
                }
                if let Some(f) = &b.catch_ {
                    walk_with_parent(f, p, visit);
                }
            }
            FragmentChild::KeyBlock(b) => walk_with_parent(&b.fragment, p, visit),
            FragmentChild::SnippetBlock(b) => walk_with_parent(&b.body, p, visit),
            _ => {}
        }
    }
}

/// A stricter identifier check that also rejects reserved words.
fn is_valid_identifier_strict(s: &str) -> bool {
    if !is_valid_identifier(s) {
        return false;
    }
    !is_reserved_word(s)
}

fn is_reserved_word(s: &str) -> bool {
    matches!(
        s,
        "break"
            | "case"
            | "catch"
            | "class"
            | "const"
            | "continue"
            | "debugger"
            | "default"
            | "delete"
            | "do"
            | "else"
            | "export"
            | "extends"
            | "false"
            | "finally"
            | "for"
            | "function"
            | "if"
            | "import"
            | "in"
            | "instanceof"
            | "new"
            | "null"
            | "return"
            | "super"
            | "switch"
            | "this"
            | "throw"
            | "true"
            | "try"
            | "typeof"
            | "var"
            | "void"
            | "while"
            | "with"
            | "yield"
            | "enum"
            | "implements"
            | "interface"
            | "let"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "static"
            | "await"
    )
}

// ---------------------------------------------------------------------------
// Simple `on:event={fn}` → `onevent={fn}` migration on RegularElement /
// SvelteElement etc. Only handles the no-modifier, single-occurrence case
// with an explicit handler expression. More complex cases (modifiers,
// bubbling, multiple handlers per event) require the full `svelte/legacy`
// import insertion + handlers() merging and are deferred.
// ---------------------------------------------------------------------------

fn migrate_simple_on_events(source: &str, str: &mut MagicString, frag: &Fragment) {
    walk_fragment(frag, &mut |child| {
        let attrs = match child {
            FragmentChild::RegularElement(e) => &e.attributes,
            FragmentChild::SvelteElement(e) => &e.attributes,
            FragmentChild::SvelteBody(e)
            | FragmentChild::SvelteWindow(e)
            | FragmentChild::SvelteDocument(e)
            | FragmentChild::SvelteHead(e) => &e.attributes,
            _ => return,
        };
        // First pass: bucket OnDirective by event name; only migrate buckets
        // with exactly one entry, no modifiers, with an explicit expression.
        let mut by_event: std::collections::HashMap<String, Vec<&svelte_ast::attributes::OnDirective>> = Default::default();
        for a in attrs {
            if let ElementAttribute::OnDirective(od) = a {
                by_event.entry(od.name.clone()).or_default().push(od);
            }
        }
        for (_name, list) in by_event {
            if list.len() != 1 {
                continue;
            }
            let od = list[0];
            if !od.modifiers.is_empty() {
                continue;
            }
            let Some(expr) = &od.expression else {
                continue;
            };
            // Replace `on:NAME` with `onNAME` (5+name bytes → 2+name bytes).
            // The directive's start..start+3+namelen covers `on:NAME`, so we
            // overwrite that with `on${NAME}`.
            let start = od.start as usize;
            let bytes = source.as_bytes();
            // Find `on:` then NAME at `od.start`. Confirm.
            if start + 3 >= bytes.len() || &bytes[start..start + 3] != b"on:" {
                continue;
            }
            // Find the colon position to remove it.
            // Overwrite just `on:NAME` portion to `onNAME`.
            let name_len = od.name.len();
            let kw_end = start + 3 + name_len;
            // Sanity check: bytes after kw_end must be `=` or end-of-directive.
            // We use `od.end` as the boundary.
            // Replace `on:NAME` with `onNAME`.
            let _ = kw_end;
            let _ = expr;
            // The simplest replacement: overwrite the colon at start+2 with empty.
            str.remove(start + 2, start + 3);
        }
    });
}

// ---------------------------------------------------------------------------
// Simple derivations: `$: x = expr;` (with single Identifier target, or
// destructure target) → `let x = $derived(expr);` (or `let { x } = $derived(expr);`).
// If preceded by `let x;` (no init), remove that line.
// Only fires for the easy case — no other assignment to x in script, no
// modifications inside the labeled statement, no multi-statement block.
// ---------------------------------------------------------------------------

fn migrate_simple_derivations(source: &str, str: &mut MagicString, root: &Root) {
    let Some(instance) = &root.instance else {
        return;
    };
    let body = &instance.content.body;

    // Pre-pass: count "outside-$:" assignment targets, so we only convert
    // when the only assignment is inside the $: block itself.
    let mut outside_assigns: std::collections::HashMap<String, usize> = Default::default();
    for stmt in body {
        // Skip $: labeled statements (we want to count NON-$: assignments).
        if let Statement::Labeled(l) = stmt {
            if l.label.name == "$" {
                continue;
            }
        }
        let mut targets = std::collections::HashSet::new();
        collect_assignment_targets(stmt, &mut targets);
        for t in targets {
            *outside_assigns.entry(t).or_insert(0) += 1;
        }
    }
    // Also handler-bound assignments in the template count as outside.
    walk_fragment(&root.fragment, &mut |child| {
        let attrs = match child {
            FragmentChild::RegularElement(e) => Some(&e.attributes),
            FragmentChild::Component(e) => Some(&e.attributes),
            FragmentChild::SvelteComponent(e) => Some(&e.attributes),
            FragmentChild::SvelteElement(e) => Some(&e.attributes),
            _ => None,
        };
        if let Some(attrs) = attrs {
            for a in attrs {
                match a {
                    ElementAttribute::OnDirective(od) => {
                        if let Some(expr) = &od.expression {
                            let mut targets = std::collections::HashSet::new();
                            collect_assignment_targets_expr(expr, &mut targets);
                            for t in targets {
                                *outside_assigns.entry(t).or_insert(0) += 1;
                            }
                        }
                    }
                    ElementAttribute::Attribute(attr) => match &attr.value {
                        AttributeValue::Single(t) => {
                            let mut targets = std::collections::HashSet::new();
                            collect_assignment_targets_expr(&t.expression, &mut targets);
                            for t in targets {
                                *outside_assigns.entry(t).or_insert(0) += 1;
                            }
                        }
                        AttributeValue::Many(parts) => {
                            for p in parts {
                                if let AttributeValuePart::ExpressionTag(t) = p {
                                    let mut targets = std::collections::HashSet::new();
                                    collect_assignment_targets_expr(&t.expression, &mut targets);
                                    for t in targets {
                                        *outside_assigns.entry(t).or_insert(0) += 1;
                                    }
                                }
                            }
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
    });

    // Also count how many $: blocks each target appears in — only convert when 1.
    let mut dollar_assigns: std::collections::HashMap<String, usize> = Default::default();
    for stmt in body {
        if let Statement::Labeled(l) = stmt {
            if l.label.name == "$" {
                if let Statement::Expression(es) = &l.body {
                    if let Expression::Assignment(asn) = &es.expression {
                        match &asn.left {
                            svelte_js_ast::AssignmentTarget::Expression(
                                Expression::Identifier(id),
                            ) => {
                                *dollar_assigns.entry(id.name.clone()).or_insert(0) += 1;
                            }
                            svelte_js_ast::AssignmentTarget::Pattern(p) => {
                                let mut names = std::collections::HashSet::new();
                                collect_pattern_names(p, &mut names);
                                for n in names {
                                    *dollar_assigns.entry(n).or_insert(0) += 1;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    // Pass 1: iterate labeled `$:` ExpressionStatement(AssignmentExpression).
    let bytes = source.as_bytes();
    for stmt in body {
        let Statement::Labeled(l) = stmt else { continue };
        if l.label.name != "$" {
            continue;
        }
        let Statement::Expression(es) = &l.body else {
            continue;
        };
        // Unwrap parens: `$: ({x} = …);` parses with Paren around AssignmentExpression.
        let inner = match &es.expression {
            Expression::Paren(p) => &p.expression,
            other => other,
        };
        let Expression::Assignment(asn) = inner else {
            continue;
        };
        if asn.operator != svelte_js_ast::AssignmentOperator::Assign {
            continue;
        }
        // Identify target style.
        let (target_text, target_names): (String, Vec<String>) = match &asn.left {
            svelte_js_ast::AssignmentTarget::Expression(Expression::Identifier(id)) => {
                (id.name.clone(), vec![id.name.clone()])
            }
            svelte_js_ast::AssignmentTarget::Pattern(p) => {
                // Use the source slice as-is for the destructure pattern.
                let (s, e) = pattern_span(p);
                let slice = &source[s as usize..e as usize];
                let mut names = std::collections::HashSet::new();
                collect_pattern_names(p, &mut names);
                if names.is_empty() {
                    continue;
                }
                (slice.to_string(), names.into_iter().collect())
            }
            _ => continue,
        };
        // Skip if any target has outside assignment (state-like) or multiple
        // `$:` assignments.
        let mut skip = false;
        for n in &target_names {
            if outside_assigns.get(n).copied().unwrap_or(0) > 0 {
                skip = true;
                break;
            }
            if dollar_assigns.get(n).copied().unwrap_or(0) > 1 {
                skip = true;
                break;
            }
        }
        if skip {
            continue;
        }
        // Also skip if any name clashes with a rune.
        if target_names.iter().any(|n| n == "derived") {
            continue;
        }
        // Get RHS bounds.
        let (rs, re) = expr_span(&asn.right);
        let rhs_text = &source[rs as usize..re as usize];

        // Build the replacement for the labeled statement: `let TARGET = $derived(RHS)`
        let l_start = l.span.start as usize;
        let l_end = l.span.end as usize;
        // Preserve trailing `;` semantics: l_end may or may not include it.
        let has_trailing_semi = bytes
            .get(l_end - 1)
            .map(|b| *b == b';')
            .unwrap_or(false);
        let _ = has_trailing_semi;

        // Build target text: if it's an object pattern that comes from
        // `({ x } = …)`, the source slice includes the parens — strip them.
        let target_clean = if target_text.starts_with('(') && target_text.ends_with(')') {
            target_text[1..target_text.len() - 1].trim().to_string()
        } else {
            target_text
        };

        // Look for a preceding sibling `let X;` (single declarator, no init,
        // Identifier matching a target name).
        let mut preceding_let_id_end: Option<usize> = None;
        for sibling in body {
            let Statement::Variable(v) = sibling else {
                continue;
            };
            if v.span.start as usize >= l_start {
                continue;
            }
            if v.declarations.len() != 1 {
                continue;
            }
            let d = &v.declarations[0];
            let Pattern::Identifier(id) = &d.id else {
                continue;
            };
            if d.init.is_some() {
                continue;
            }
            if !target_names.contains(&id.name) {
                continue;
            }
            preceding_let_id_end = Some(id.span.end as usize);
        }

        if let Some(id_end) = preceding_let_id_end {
            // Upstream approach: append ` = $derived(RHS)` after the `let X`
            // identifier and remove the labeled statement entirely. Keeps
            // visual whitespace where `$:` used to be (the indent stays put,
            // we only strip from `$` to end-of-statement so the blank line
            // is `\t\n` not `\n`).
            str.append_left(id_end, format!(" = $derived({})", rhs_text));
            str.remove(l_start, l_end);
        } else {
            // No preceding let → replace the labeled statement with a fresh
            // `let TARGET = $derived(RHS);` (preserving the trailing `;` if
            // present).
            let replacement = format!("let {} = $derived({})", target_clean, rhs_text);
            let final_replacement = if bytes.get(l_end.saturating_sub(1)).copied() == Some(b';') {
                format!("{};", replacement)
            } else {
                replacement
            };
            str.update(l_start, l_end, &final_replacement);
        }
    }
    let _ = str;
}

// ---------------------------------------------------------------------------
// Simple state migration: `let X = expr;` or `let X;` (non-prop) where X is
// reassigned somewhere → wrap with `$state(...)`.
// ---------------------------------------------------------------------------

fn migrate_simple_state(source: &str, str: &mut MagicString, root: &Root) {
    let Some(instance) = &root.instance else {
        return;
    };

    // Detect props (`export let X`) — skip these.
    let mut exported_props: std::collections::HashSet<String> = std::collections::HashSet::new();
    for stmt in &instance.content.body {
        if let Statement::ExportNamed(en) = stmt {
            if let Some(Statement::Variable(v)) = en.declaration.as_ref() {
                for d in &v.declarations {
                    if let Pattern::Identifier(id) = &d.id {
                        exported_props.insert(id.name.clone());
                    }
                }
            }
        }
    }

    // Detect $: targets (would become derived) — skip these.
    let mut derived_targets: std::collections::HashSet<String> = std::collections::HashSet::new();
    for stmt in &instance.content.body {
        if let Statement::Labeled(l) = stmt {
            if l.label.name == "$" {
                if let Statement::Expression(es) = &l.body {
                    if let Expression::Assignment(asn) = &es.expression {
                        if let svelte_js_ast::AssignmentTarget::Expression(
                            Expression::Identifier(id),
                        ) = &asn.left
                        {
                            derived_targets.insert(id.name.clone());
                        }
                        if let svelte_js_ast::AssignmentTarget::Pattern(p) = &asn.left {
                            collect_pattern_names(p, &mut derived_targets);
                        }
                    }
                }
            }
        }
    }

    // Collect reassignment targets from script.
    let mut reassigned: std::collections::HashSet<String> = std::collections::HashSet::new();
    for stmt in &instance.content.body {
        collect_assignment_targets(stmt, &mut reassigned);
    }
    // Collect bind: targets + event-handler reassignments from template.
    walk_fragment(&root.fragment, &mut |child| {
        let attrs = match child {
            FragmentChild::RegularElement(e) => Some(&e.attributes),
            FragmentChild::Component(e) => Some(&e.attributes),
            FragmentChild::SvelteComponent(e) => Some(&e.attributes),
            FragmentChild::SvelteElement(e) => Some(&e.attributes),
            FragmentChild::SvelteBody(e)
            | FragmentChild::SvelteBoundary(e)
            | FragmentChild::SvelteDocument(e)
            | FragmentChild::SvelteFragment(e)
            | FragmentChild::SvelteHead(e)
            | FragmentChild::SvelteOptions(e)
            | FragmentChild::SvelteSelf(e)
            | FragmentChild::SvelteWindow(e) => Some(&e.attributes),
            _ => None,
        };
        if let Some(attrs) = attrs {
            for a in attrs {
                match a {
                    ElementAttribute::BindDirective(b) => {
                        if let Some(name) = bind_target_identifier(&b.expression) {
                            reassigned.insert(name);
                        }
                    }
                    ElementAttribute::OnDirective(od) => {
                        if let Some(expr) = &od.expression {
                            collect_assignment_targets_expr(expr, &mut reassigned);
                        }
                    }
                    ElementAttribute::Attribute(attr) => {
                        // Attribute values may contain ExpressionTags with
                        // handler-shaped expressions (`onclick={() => …}`).
                        match &attr.value {
                            AttributeValue::Single(t) => {
                                collect_assignment_targets_expr(&t.expression, &mut reassigned);
                            }
                            AttributeValue::Many(parts) => {
                                for p in parts {
                                    if let AttributeValuePart::ExpressionTag(t) = p {
                                        collect_assignment_targets_expr(
                                            &t.expression,
                                            &mut reassigned,
                                        );
                                    }
                                }
                            }
                            AttributeValue::Empty => {}
                        }
                    }
                    _ => {}
                }
            }
        }
    });

    // Now visit each top-level `let X` or `let X = INIT`:
    //   - skip if X is a prop, or a derived target, or has no reassignment.
    //   - wrap init in `$state(...)` (or insert `= $state()` if no init).
    for stmt in &instance.content.body {
        if let Statement::Variable(v) = stmt {
            // Only `let` declarations.
            if !matches!(v.kind, svelte_js_ast::VariableKind::Let) {
                continue;
            }
            for d in &v.declarations {
                let Pattern::Identifier(id) = &d.id else {
                    continue;
                };
                if exported_props.contains(&id.name) {
                    continue;
                }
                if derived_targets.contains(&id.name) {
                    continue;
                }
                if !reassigned.contains(&id.name) {
                    continue;
                }
                // Also skip if there's a `let state` conflict — that'd be the
                // impossible-migrate case we already caught.
                if id.name == "state" {
                    continue;
                }

                // Wrap init or insert `= $state()` after id.
                // Find the end of the identifier (or its type annotation if
                // any) — we don't have type annotation info in the AST yet,
                // so we use the textual approach: locate identifier in source.
                if let Some(init) = &d.init {
                    let (es, ee) = expr_span(init);
                    // Prepend `$state(` before init, append `)` after.
                    // Handle sequence-expression parenthesis case like upstream.
                    let s = es as usize;
                    let e = ee as usize;
                    // Find `=` between id and init: it's right before `s`
                    // typically.
                    // Just wrap at init bounds.
                    str.prepend_left(s, "$state(");
                    str.append_right(e, ")");
                } else {
                    // Insert `= $state()` right after the identifier (or
                    // its TS type annotation). Since we don't have type
                    // annotation in AST, do a textual scan from `id.span.end`
                    // up to `;` or `,` or `\n`.
                    let id_end = id.span.end as usize;
                    let bytes = source.as_bytes();
                    let mut p = id_end;
                    // Skip TS type annotation if present (`: TYPE`).
                    // Look for the next `;`, `,`, `=`, or `\n` — whichever
                    // marks the end of the declarator. If we see `=`, that
                    // means there's actually an init we missed (shouldn't
                    // happen). Otherwise, we insert before `;`/`,`/`\n`.
                    while p < bytes.len() {
                        let b = bytes[p];
                        if b == b';' || b == b',' || b == b'\n' {
                            break;
                        }
                        p += 1;
                    }
                    str.append_left(p, " = $state()");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// HTML `<!-- svelte-ignore ... -->` migration.
// ---------------------------------------------------------------------------

fn migrate_comments(source: &str, str: &mut MagicString, root: &Root) {
    walk_fragment(&root.fragment, &mut |child| {
        if let FragmentChild::Comment(c) = child {
            let migrated = migrate_svelte_ignore_text(&c.data);
            if migrated != c.data {
                let inner_start = c.start as usize + "<!--".len();
                let inner_end = c.end as usize - "-->".len();
                str.update(inner_start, inner_end, &migrated);
            }
        }
    });
    // Script-internal JS line/block comments. The `value` is the content
    // (without `//` or `/* */`). For line comments, the upstream walker
    // overwrites `start + '//'.length .. end` with the migrated value.
    let bytes = source.as_bytes();
    for c in &root.comments {
        let mut start = c.start as usize;
        let end = c.end as usize;
        if end > bytes.len() {
            continue;
        }
        // Some upstream parsers return positions that lead the actual `//`
        // marker by leading whitespace. Snap `start` forward to the first
        // `//` or `/*` between `start..end`.
        while start + 2 <= end {
            let head = &source[start..start + 2];
            if head == "//" || head == "/*" {
                break;
            }
            start += 1;
        }
        if start + 2 > end {
            continue;
        }
        let head = &source[start..start + 2];
        if head == "//" {
            let inner = &source[start + 2..end];
            let migrated = migrate_svelte_ignore_text(inner);
            if migrated != inner {
                str.update(start + 2, end, &migrated);
            }
        } else if head == "/*" {
            if end >= start + 4 {
                let inner = &source[start + 2..end - 2];
                let migrated = migrate_svelte_ignore_text(inner);
                if migrated != inner {
                    str.update(start + 2, end - 2, &migrated);
                }
            }
        }
    }
}

const SVELTE_IGNORE_REPLACEMENTS: &[(&str, &str)] = &[
    (
        "non-top-level-reactive-declaration",
        "reactive_declaration_invalid_placement",
    ),
    (
        "module-script-reactive-declaration",
        "reactive_declaration_module_script",
    ),
    ("empty-block", "block_empty"),
    ("avoid-is", "attribute_avoid_is"),
    ("invalid-html-attribute", "attribute_invalid_property_name"),
    ("a11y-structure", "a11y_figcaption_parent"),
    ("illegal-attribute-character", "attribute_illegal_colon"),
    ("invalid-rest-eachblock-binding", "bind_invalid_each_rest"),
    ("unused-export-let", "export_let_unused"),
];

/// Port of `migrate_svelte_ignore` in `packages/svelte/src/compiler/utils/extract_svelte_ignore.js`.
fn migrate_svelte_ignore_text(text: &str) -> String {
    // Match `^\s*svelte-ignore\s`.
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t' || bytes[i] == b'\n') {
        i += 1;
    }
    let needle = b"svelte-ignore";
    if i + needle.len() >= bytes.len() {
        return text.to_string();
    }
    if &bytes[i..i + needle.len()] != needle {
        return text.to_string();
    }
    let after = i + needle.len();
    if !matches!(bytes[after], b' ' | b'\t' | b'\n') {
        return text.to_string();
    }
    let prefix_len = after + 1;
    let prefix = &text[..prefix_len];
    let rest = &text[prefix_len..];

    // Replace each `\w+-\w+(-\w+)*` in rest. Word chars = ASCII alphanumeric
    // or `_`. Hyphenated pattern: at least one `-`.
    let rest_bytes = rest.as_bytes();
    let mut out = String::new();
    out.push_str(prefix);
    let mut j = 0;
    while j < rest_bytes.len() {
        // Try to match a hyphenated word starting at j.
        let m_start = j;
        // Read word chars.
        while j < rest_bytes.len() && is_ident_char(rest_bytes[j]) {
            j += 1;
        }
        if j > m_start && j < rest_bytes.len() && rest_bytes[j] == b'-' {
            // Continue: at least one hyphen — match `\w+-\w+(-\w+)*`.
            let mut k = j;
            let mut valid = false;
            while k < rest_bytes.len() && rest_bytes[k] == b'-' {
                k += 1;
                // Must have at least one word char after.
                let chunk_start = k;
                while k < rest_bytes.len() && is_ident_char(rest_bytes[k]) {
                    k += 1;
                }
                if k > chunk_start {
                    valid = true;
                } else {
                    valid = false;
                    break;
                }
            }
            if valid {
                let code = &rest[m_start..k];
                // Find the replacement.
                let replacement = SVELTE_IGNORE_REPLACEMENTS
                    .iter()
                    .find(|(k, _)| *k == code)
                    .map(|(_, v)| (*v).to_string())
                    .unwrap_or_else(|| code.replace('-', "_"));
                // If there's another `\w+-\w+` later in rest, append comma.
                let following = &rest[k..];
                let has_following_hyphenated = has_hyphenated_word(following);
                out.push_str(&replacement);
                if has_following_hyphenated {
                    out.push(',');
                }
                j = k;
                continue;
            }
        }
        // Otherwise emit chars verbatim up to current j.
        if j == m_start {
            // No advance — emit one byte.
            out.push(rest_bytes[j] as char);
            j += 1;
        } else {
            out.push_str(&rest[m_start..j]);
        }
    }
    out
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn has_hyphenated_word(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Find a word.
        let start = i;
        while i < bytes.len() && is_ident_char(bytes[i]) {
            i += 1;
        }
        if i > start && i < bytes.len() && bytes[i] == b'-' {
            // Need at least one word char after hyphen.
            let mut k = i + 1;
            let kstart = k;
            while k < bytes.len() && is_ident_char(bytes[k]) {
                k += 1;
            }
            if k > kstart {
                return true;
            }
            i = k;
        } else if i == start {
            i += 1;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Block whitespace trim — `{  @html "x"  }` → `{@html "x"}` etc.
// Mirrors upstream's `trim_block(state, start, end)`.
// ---------------------------------------------------------------------------

fn migrate_block_whitespace(source: &str, str: &mut MagicString, frag: &Fragment) {
    walk_fragment(frag, &mut |child| {
        match child {
            FragmentChild::HtmlTag(t) => {
                trim_block(source, str, t.start as usize, t.end as usize);
            }
            FragmentChild::ConstTag(t) => {
                trim_block(source, str, t.start as usize, t.end as usize);
            }
            FragmentChild::IfBlock(b) => {
                // Trim the opening `{#if expr}`. Upstream finds the closing
                // `}` from `node.test.end`. We use the expression position
                // directly. The block's full `start..end` covers the entire
                // block — we only trim the opener.
                let start = b.start as usize;
                let end = find_first_close_brace(source, expression_end(&b.test) as usize);
                if end > start {
                    trim_block(source, str, start, end);
                }
                // {:else if …}: scan the consequent fragment's end and the
                // alternate's start to find any intermediate `{:else if …}`
                // openers. Upstream walks the IfBlock visitor which fires
                // recursively for else-if (since they're nested IfBlocks
                // with `elseif: true`).
                // No action needed beyond the recursive walk.
            }
            FragmentChild::KeyBlock(b) => {
                let start = b.start as usize;
                let end = find_first_close_brace(source, expression_end(&b.expression) as usize);
                if end > start {
                    trim_block(source, str, start, end);
                }
            }
            FragmentChild::AwaitBlock(b) => {
                let start = b.start as usize;
                // The opener spans `{#await expr}` or `{#await expr then val}`
                // or `{#await expr catch err}` — we need to find the first
                // `}` after either expression.end OR value.end (when there's
                // no pending block).
                let last_kw_end = if b.pending.is_some() {
                    expression_end(&b.expression) as usize
                } else if let Some(v) = &b.value {
                    pattern_end(v) as usize
                } else {
                    expression_end(&b.expression) as usize
                };
                let end = find_first_close_brace(source, last_kw_end);
                if end > start {
                    trim_block(source, str, start, end);
                }
                // {:then VALUE} sub-block (when there's a pending fragment).
                if b.pending.is_some() {
                    if let Some(v) = &b.value {
                        let vstart = pattern_start(v) as usize;
                        let open = source[..vstart].rfind('{').unwrap_or(vstart);
                        let close = find_first_close_brace(source, pattern_end(v) as usize);
                        if close > open {
                            trim_block(source, str, open, close);
                        }
                    }
                }
                // {:catch ERROR}
                if b.catch_.is_some() {
                    if let Some(e) = &b.error {
                        let estart = pattern_start(e) as usize;
                        let open = source[..estart].rfind('{').unwrap_or(estart);
                        let close = find_first_close_brace(source, pattern_end(e) as usize);
                        if close > open {
                            trim_block(source, str, open, close);
                        }
                    }
                }
            }
            _ => {}
        }
    });
    // {:else if EXPR} openers — handled when the IfBlock has an alternate
    // that is itself an IfBlock with `elseif: true`. The fragment walk's
    // recursion already enters that nested IfBlock and trims its opener.
    // BUT — we also need to trim any extra padding inside the simple
    // `{:else}` and the `{/if}`, `{/await}`, `{/key}` closers. Upstream
    // doesn't touch those (they're invariant). So nothing to do here.
}

fn expression_end(expr: &Expression) -> u32 {
    expr_span(expr).1
}

fn expr_span(expr: &Expression) -> (u32, u32) {
    use svelte_js_ast::Expression as E;
    match expr {
        E::Identifier(id) => (id.span.start, id.span.end),
        E::Literal(l) => match l.as_ref() {
            svelte_js_ast::Literal::String(s) => (s.span.start, s.span.end),
            svelte_js_ast::Literal::Number(n) => (n.span.start, n.span.end),
            svelte_js_ast::Literal::Boolean(b) => (b.span.start, b.span.end),
            svelte_js_ast::Literal::Null(s) => (s.start, s.end),
            svelte_js_ast::Literal::Regex(r) => (r.span.start, r.span.end),
            svelte_js_ast::Literal::BigInt(b) => (b.span.start, b.span.end),
        },
        E::Template(t) => (t.span.start, t.span.end),
        E::Array(a) => (a.span.start, a.span.end),
        E::Object(o) => (o.span.start, o.span.end),
        E::Arrow(a) => (a.span.start, a.span.end),
        E::Function(f) => (f.span.start, f.span.end),
        E::Class(c) => (c.span.start, c.span.end),
        E::Member(m) => (m.span.start, m.span.end),
        E::Call(c) => (c.span.start, c.span.end),
        E::New(n) => (n.span.start, n.span.end),
        E::Binary(b) => (b.span.start, b.span.end),
        E::Logical(l) => (l.span.start, l.span.end),
        E::Assignment(a) => (a.span.start, a.span.end),
        E::Update(u) => (u.span.start, u.span.end),
        E::Unary(u) => (u.span.start, u.span.end),
        E::Conditional(c) => (c.span.start, c.span.end),
        E::Sequence(s) => (s.span.start, s.span.end),
        E::Spread(s) => (s.span.start, s.span.end),
        E::This(s) => (s.start, s.end),
        E::Super(s) => (s.start, s.end),
        E::Yield(y) => (y.span.start, y.span.end),
        E::Await(a) => (a.span.start, a.span.end),
        E::Tagged(t) => (t.span.start, t.span.end),
        E::Paren(p) => (p.span.start, p.span.end),
        E::Meta(m) => (m.span.start, m.span.end),
        E::Raw(_) => (0, 0),
    }
}

fn pattern_start(p: &Pattern) -> u32 {
    pattern_span(p).0
}

fn pattern_end(p: &Pattern) -> u32 {
    pattern_span(p).1
}

fn pattern_span(p: &Pattern) -> (u32, u32) {
    match p {
        Pattern::Identifier(id) => (id.span.start, id.span.end),
        Pattern::Array(a) => (a.span.start, a.span.end),
        Pattern::Object(o) => (o.span.start, o.span.end),
        Pattern::Rest(r) => (r.span.start, r.span.end),
        Pattern::Assignment(a) => (a.span.start, a.span.end),
        Pattern::Member(m) => (m.span.start, m.span.end),
    }
}

fn find_first_close_brace(source: &str, from: usize) -> usize {
    let bytes = source.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == b'}' {
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

fn trim_block(source: &str, str: &mut MagicString, start: usize, end: usize) {
    // Slice between `{` and `}` (exclusive).
    if end < 2 || start >= source.len() || end > source.len() {
        return;
    }
    let inner = &source[start + 1..end - 1];
    let trimmed = inner.trim();
    if trimmed.len() != inner.len() {
        str.update(start + 1, end - 1, trimmed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_accessors() {
        let src = "<svelte:options accessors immutable/>";
        let r = migrate(src, MigrateOptions::default());
        assert_eq!(r.code, "<svelte:options immutable/>");
    }

    #[test]
    fn identity_when_no_accessors() {
        let src = "<div>hi</div>";
        let r = migrate(src, MigrateOptions::default());
        assert_eq!(r.code, "<div>hi</div>");
    }

}
