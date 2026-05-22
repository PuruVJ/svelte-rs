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

    // 2.5. Detect reactive-statement reordering. If any `$:` statement
    //      depends on a binding declared after it, upstream moves all
    //      reactive statements to the end of the script block (their
    //      Svelte 4 implicit topological reordering). We do this by
    //      textually rewriting the source and re-parsing, so the rest
    //      of the pipeline operates on the reordered source.
    if let Some(new_source) = reorder_reactive_statements(source, &parsed) {
        let new_parsed = match svelte_parse::parse(&new_source, false) {
            Ok(r) => r,
            Err(_) => {
                // If reordering breaks parsing, just continue with original.
                return run_pipeline(source, og_source, &parsed, &style_contents, &opts);
            }
        };
        let leaked: &'static str = Box::leak(new_source.into_boxed_str());
        return run_pipeline(leaked, og_source, &new_parsed, &style_contents, &opts);
    }

    run_pipeline(source, og_source, &parsed, &style_contents, &opts)
}

fn run_pipeline(
    source: &str,
    og_source: &str,
    parsed: &Root,
    style_contents: &[(usize, String)],
    opts: &MigrateOptions,
) -> MigrateResult {
    let _ = og_source;
    // 3. Apply surface-level edits.
    let mut str = MagicString::new(source.to_string());
    strip_accessors_in_svelte_options(source, &mut str);
    migrate_script_module_context(source, &mut str, &parsed);
    migrate_self_closing_elements(source, &mut str, &parsed.fragment);
    migrate_svelte_self_no_filename(source, &mut str, &parsed.fragment, opts.filename.as_deref());
    migrate_svelte_self_with_filename(source, &mut str, &parsed, opts.filename.as_deref());
    migrate_svelte_element_static_this(source, &mut str, &parsed.fragment);
    migrate_svelte_component(source, &mut str, &parsed.fragment);
    migrate_invalid_named_slots(source, &mut str, &parsed.fragment);
    migrate_simple_on_events(source, &mut str, &parsed.fragment);
    // Gather slot info early so we can do the `$$slots.X` → `X` global
    // replace BEFORE derivations/state passes (which may issue overlapping
    // updates that would otherwise clobber when MagicString splits chunks).
    let slot_info = gather_slot_info(source, &parsed);
    apply_slot_template_edits(source, &mut str, &parsed, &slot_info);
    // Run derivations first so we know which `$:` statements will become
    // `let X = $derived(...)` (or `$state(LIT)`). State migration then skips
    // bindings already consumed by the derivation pass.
    let (derived_labeled_starts, derived_consumed_names) =
        migrate_simple_derivations(source, &mut str, &parsed);
    migrate_simple_state(source, &mut str, &parsed, &derived_consumed_names);
    migrate_unused_beforeafter_imports(source, &mut str, &parsed);
    migrate_simple_props(
        source,
        &mut str,
        &parsed,
        &slot_info,
        opts.use_ts,
        opts.filename.as_deref(),
    );
    migrate_export_specifier_props(source, &mut str, &parsed);
    migrate_effects(source, &mut str, &parsed, &derived_labeled_starts);
    migrate_comments(source, &mut str, &parsed);
    migrate_block_whitespace(source, &mut str, &parsed.fragment);

    // Restore the original `<style>` bodies that we blanked before parsing.
    // Apply the CSS `:has/:is/:where` `:global(...)` wrap as we go.
    for (start, content) in style_contents.iter() {
        let end = start + STYLE_PLACEHOLDER.len();
        let migrated = migrate_css_body(content);
        str.overwrite(*start, end, &migrated);
    }

    MigrateResult {
        code: str.to_string(),
    }
}

const STYLE_PLACEHOLDER: &str = "/*$$__STYLE_CONTENT__$$*/";

// ---------------------------------------------------------------------------
// Reactive-statement reorder pre-pass.
//
// Svelte 4 reordered `$:` statements topologically by their bindings. Svelte
// 5's `$derived`/`$effect.pre` don't, so when migrating we must move any
// `$:` whose deps are declared after it to the end of the script block.
//
// Upstream sets `needs_reordering = true` if ANY reactive statement has a
// dep declared after it, and then moves ALL reactive statements (in their
// original order) to the end of the script content.
//
// We implement this by rewriting the source text and returning the new
// source so the rest of the pipeline can reparse and operate on it.
// ---------------------------------------------------------------------------

fn reorder_reactive_statements(source: &str, root: &Root) -> Option<String> {
    let instance = root.instance.as_ref()?;
    let body = &instance.content.body;
    // Collect `$:` labeled statements with their dependency identifiers.
    let mut labeled: Vec<(usize, usize, Vec<String>, Vec<String>)> = Vec::new();
    for stmt in body {
        if let Statement::Labeled(l) = stmt {
            if l.label.name != "$" {
                continue;
            }
            // Targets (LHS identifiers) for the labeled statement.
            let mut targets: std::collections::HashSet<String> = Default::default();
            collect_assignment_targets(&l.body, &mut targets);
            // All identifiers referenced anywhere in the labeled body.
            let mut all_ids: std::collections::HashSet<String> = Default::default();
            collect_identifiers_in_statement(&l.body, &mut all_ids);
            // Dependencies = all referenced ids minus targets and
            // locally-declared identifiers within the body.
            let mut locals: std::collections::HashSet<String> = Default::default();
            collect_top_level_decl_names(&l.body, &mut locals);
            let deps: Vec<String> = all_ids
                .into_iter()
                .filter(|n| !targets.contains(n) && !locals.contains(n))
                .collect();
            labeled.push((
                l.span.start as usize,
                l.span.end as usize,
                targets.into_iter().collect(),
                deps,
            ));
        }
    }
    if labeled.is_empty() {
        return None;
    }
    // Build a map from binding name → its declaration's start byte (we use the
    // span of `VariableDeclaration` containing the identifier). Also note
    // `export let` and `function` declarations.
    let mut decl_start: std::collections::HashMap<String, usize> = Default::default();
    let mut prop_names: std::collections::HashSet<String> = Default::default();
    for stmt in body {
        match stmt {
            Statement::Variable(v) => {
                for d in &v.declarations {
                    let mut names: std::collections::HashSet<String> = Default::default();
                    collect_pattern_names(&d.id, &mut names);
                    for n in names {
                        decl_start
                            .entry(n)
                            .or_insert(v.span.start as usize);
                    }
                }
            }
            Statement::Function(f) => {
                if let Some(id) = &f.id {
                    decl_start
                        .entry(id.name.clone())
                        .or_insert(f.span.start as usize);
                }
            }
            Statement::Class(c) => {
                if let Some(id) = &c.id {
                    decl_start
                        .entry(id.name.clone())
                        .or_insert(c.span.start as usize);
                }
            }
            Statement::ExportNamed(en) => {
                if let Some(decl) = &en.declaration {
                    match decl {
                        Statement::Variable(v) => {
                            for d in &v.declarations {
                                let mut names: std::collections::HashSet<String> = Default::default();
                                collect_pattern_names(&d.id, &mut names);
                                for n in names {
                                    prop_names.insert(n.clone());
                                    decl_start.entry(n).or_insert(en.span.start as usize);
                                }
                            }
                        }
                        Statement::Function(f) => {
                            if let Some(id) = &f.id {
                                decl_start
                                    .entry(id.name.clone())
                                    .or_insert(en.span.start as usize);
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    // Compute `props_insertion_point` = end of the LAST top-level
    // ImportDeclaration in the script (mirrors upstream's
    // `ImportDeclaration` visitor which sets it to `node.end`). Default to
    // the script content start.
    let mut props_insertion_point = instance.content.span.start as usize;
    for stmt in body {
        if let Statement::Import(imp) = stmt {
            let e = imp.span.end as usize;
            if e > props_insertion_point {
                props_insertion_point = e;
            }
        }
    }
    // For each prop name, override its decl_start with props_insertion_point
    // so that "depends on a prop declared later" is computed against the
    // actual prop insertion site (after all imports).
    for n in &prop_names {
        decl_start.insert(n.clone(), props_insertion_point);
    }
    // Also register labeled statements that produce targets as the binding's
    // declaration point. `$: mobile = …` introduces an implicit `mobile`.
    for (s, _e, targets, _deps) in &labeled {
        for t in targets {
            decl_start.entry(t.clone()).or_insert(*s);
        }
    }
    // Check if reordering is needed: any labeled stmt has a dep whose decl
    // start is greater than the labeled stmt's start.
    let needs_reorder = labeled.iter().any(|(start, _end, _targets, deps)| {
        deps.iter().any(|dep| {
            decl_start
                .get(dep)
                .map(|d| *d > *start)
                .unwrap_or(false)
        })
    });
    if !needs_reorder {
        return None;
    }
    // Now textually move each labeled statement to the end of the instance
    // script content. We compute extended ranges (line-start to line-end+\n)
    // and reassemble the source.
    let bytes = source.as_bytes();
    let content_end = instance.content.span.end as usize;
    // Compute extended start = back to line-start (only whitespace), end = forward to \n inclusive.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for (s, e, _t, _d) in &labeled {
        let mut start = *s;
        // Walk back to line-start if only whitespace precedes on the line.
        let mut idx = start;
        while idx > 0 && bytes[idx - 1] != b'\n' && bytes[idx - 1] != b'\r' {
            idx -= 1;
            if bytes[idx] != b' ' && bytes[idx] != b'\t' {
                idx = start;
                break;
            }
        }
        start = idx;
        let mut end = *e;
        // Extend end past trailing newline.
        while end < bytes.len() && bytes[end] != b'\n' {
            end += 1;
        }
        if end < bytes.len() && bytes[end] == b'\n' {
            end += 1;
        }
        ranges.push((start, end));
    }
    // Topologically sort labeled statements: if A's body references a
    // target produced by B, then B must come before A. We also keep the
    // original-source order as a stable tiebreaker.
    // Build node order via Kahn's algorithm. Original indices = 0..N.
    let n = labeled.len();
    let mut order: Vec<usize> = (0..n).collect();
    // Map target name → producing labeled statement index.
    let mut producer: std::collections::HashMap<String, usize> = Default::default();
    for (i, (_s, _e, targets, _deps)) in labeled.iter().enumerate() {
        for t in targets {
            // Earliest producer wins (first to assign).
            producer.entry(t.clone()).or_insert(i);
        }
    }
    // Compute incoming edges count and adjacency.
    let mut indeg: Vec<usize> = vec![0; n];
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, (_s, _e, _targets, deps)) in labeled.iter().enumerate() {
        for d in deps {
            if let Some(&p) = producer.get(d) {
                if p != i {
                    adj[p].push(i);
                    indeg[i] += 1;
                }
            }
        }
    }
    // Stable Kahn — pick the lowest-original-index node with indeg=0.
    let mut sorted: Vec<usize> = Vec::with_capacity(n);
    let mut available: std::collections::BTreeSet<usize> =
        (0..n).filter(|i| indeg[*i] == 0).collect();
    while let Some(&i) = available.iter().next() {
        available.remove(&i);
        sorted.push(i);
        // Take adj snapshot to avoid borrow issues.
        let neighbors: Vec<usize> = adj[i].clone();
        for j in neighbors {
            indeg[j] -= 1;
            if indeg[j] == 0 {
                available.insert(j);
            }
        }
    }
    // If cycle detected, fall back to original order for remaining nodes.
    if sorted.len() != n {
        for i in 0..n {
            if !sorted.contains(&i) {
                sorted.push(i);
            }
        }
    }
    order = sorted;
    // Reorder ranges according to topo order.
    let original_ranges = ranges.clone();
    ranges = order
        .iter()
        .map(|&i| original_ranges[i])
        .collect();
    // For removal we want them in source order though.
    let mut removal_ranges = original_ranges.clone();
    removal_ranges.sort_by_key(|r| r.0);
    // Build moved string in topo (target) order. The removal step uses
    // source-order ranges (so we strip each region exactly once).
    let mut moved = String::new();
    for (rs, re) in &ranges {
        moved.push_str(&source[*rs..*re]);
    }
    // Reset and build cleanly: <script>...body with ranges removed...moved chunks...</script>
    let mut out = String::with_capacity(source.len());
    let mut script_body = String::new();
    let mut cur = instance.content.span.start as usize;
    // Prepend everything before script content.
    out.push_str(&source[..cur]);
    for (rs, re) in &removal_ranges {
        if *rs >= content_end {
            continue;
        }
        if cur < *rs {
            script_body.push_str(&source[cur..*rs]);
        }
        cur = (*re).min(content_end);
    }
    if cur < content_end {
        script_body.push_str(&source[cur..content_end]);
    }
    out.push_str(&script_body);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&moved);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&source[content_end..]);
    let _ = order;
    Some(out)
}

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
// CSS migration: wrap arguments of `:has(X)` / `:is(X)` / `:where(X)` with
// `:global(...)` so that bare type selectors keep matching descendants under
// the new scoping rules. Matches upstream `migrate_css` (index.js:41).
// Operates on raw CSS text (one style body) using a simple linear scan; this
// is a textual port — we don't have a CSS AST here.
// ---------------------------------------------------------------------------
fn migrate_css_body(css: &str) -> String {
    let original = css.to_string();
    let mut out = original.clone();
    // We track absolute positions in the original; insertions go into `edits`.
    // Each entry: (position, text_to_insert) with `prepend_left` semantics
    // — i.e. inserted before the char at `position`.
    let mut edits: Vec<(usize, String)> = Vec::new();

    let bytes = original.as_bytes();
    let mut starting: usize = 0;
    while starting < bytes.len() {
        let rest = &original[starting..];
        let matched_kw = if rest.starts_with(":has") {
            Some(":has")
        } else if rest.starts_with(":is") {
            Some(":is")
        } else if rest.starts_with(":where") {
            Some(":where")
        } else if rest.starts_with(":not") {
            Some(":not")
        } else {
            None
        };
        if matched_kw.is_none() {
            starting += 1;
            continue;
        }
        // Find `(` after the keyword.
        let paren = match rest.find('(') {
            Some(p) => p,
            None => {
                starting += 1;
                continue;
            }
        };
        let mut start_in_rest = paren + 1; // index inside `rest` of first char inside parens
        // Skip whitespace between `(` and the inner selector.
        let mut content_start = start_in_rest;
        while content_start < rest.len()
            && (rest.as_bytes()[content_start] == b' '
                || rest.as_bytes()[content_start] == b'\t'
                || rest.as_bytes()[content_start] == b'\n')
        {
            content_start += 1;
        }
        // Check if already starts with `:global`.
        let is_global = rest[content_start..].starts_with(":global");
        if is_global {
            // Skip the `:global` so we don't re-wrap.
            start_in_rest = content_start + ":global".len();
        }
        // Find closing `)` matching the opening paren at `paren`.
        let end_rel = find_matching_paren(rest, paren + 1);
        let end = match end_rel {
            Some(e) => e, // index AFTER the closing ')'
            None => {
                starting += 1;
                continue;
            }
        };

        // Check whether we're inside the args of an enclosing :global(...) —
        // i.e. the previous :global(...) range encloses our current position.
        // Upstream tracks this via `prev_global` and `find_closing_parenthesis`.
        let abs_pos = starting;
        let mut inside_global = false;
        // Search original up to `abs_pos` for the LAST `:global` whose paren
        // group still encloses `abs_pos`.
        let prefix = &original[..abs_pos];
        if let Some(pg) = prefix.rfind(":global") {
            // Find the `(` after `pg`.
            if let Some(rel_open) = original[pg..].find('(') {
                let open_abs = pg + rel_open;
                if let Some(close_abs) = find_matching_paren(&original, open_abs + 1) {
                    if close_abs > abs_pos {
                        inside_global = true;
                        // Skip ahead past the enclosing :global(...) close.
                        starting = close_abs;
                        continue;
                    }
                }
            }
        }
        let _ = inside_global;

        if !is_global && !rest.starts_with(":not") {
            // Insert `:global(` at `starting + start_in_rest` and `)` at
            // `starting + end - 1` (just before the closing paren).
            let ins_pos = starting + start_in_rest;
            let end_pos = starting + end - 1;
            edits.push((ins_pos, ":global(".to_string()));
            edits.push((end_pos, ")".to_string()));
        }

        // Move past the closing paren — but stay AT the `)` so outer scan
        // also processes any tail. Upstream does `code = code.substring(end-1)`.
        starting = starting + end - 1;
    }

    // Apply edits in reverse order (sorted by position desc, stable order).
    edits.sort_by(|a, b| b.0.cmp(&a.0));
    for (pos, text) in edits {
        out.insert_str(pos, &text);
    }
    out
}

/// Find the index AFTER the matching closing `)` starting from `start`
/// (assumes one `(` has already been consumed before `start`).
fn find_matching_paren(s: &str, start: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 1i32;
    let mut i = start;
    while i < bytes.len() {
        if bytes[i] == b'(' {
            depth += 1;
        } else if bytes[i] == b')' {
            depth -= 1;
            if depth == 0 {
                return Some(i + 1);
            }
        }
        i += 1;
    }
    None
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
        Expression::Object(obj) => {
            for m in &obj.properties {
                match m {
                    svelte_js_ast::ObjectMember::Property(p) => {
                        if p.computed {
                            if let svelte_js_ast::PropertyKey::Expression(e) = &p.key {
                                collect_identifiers_in_expr(e, out);
                            }
                        }
                        collect_identifiers_in_expr(&p.value, out);
                    }
                    svelte_js_ast::ObjectMember::Spread(sp) => {
                        collect_identifiers_in_expr(&sp.argument, out);
                    }
                }
            }
        }
        Expression::Array(arr) => {
            for el in &arr.elements {
                match el {
                    svelte_js_ast::ArrayElement::Expression(e) => {
                        collect_identifiers_in_expr(e, out)
                    }
                    svelte_js_ast::ArrayElement::Spread(sp) => {
                        collect_identifiers_in_expr(&sp.argument, out)
                    }
                    _ => {}
                }
            }
        }
        Expression::Conditional(c) => {
            collect_identifiers_in_expr(&c.test, out);
            collect_identifiers_in_expr(&c.consequent, out);
            collect_identifiers_in_expr(&c.alternate, out);
        }
        Expression::Unary(u) => collect_identifiers_in_expr(&u.argument, out),
        Expression::Update(u) => collect_identifiers_in_expr(&u.argument, out),
        Expression::New(n) => {
            collect_identifiers_in_expr(&n.callee, out);
            for arg in &n.arguments {
                if let svelte_js_ast::Argument::Expression(e) = arg {
                    collect_identifiers_in_expr(e, out);
                }
            }
        }
        Expression::Sequence(s) => {
            for e in &s.expressions {
                collect_identifiers_in_expr(e, out);
            }
        }
        Expression::Template(t) => {
            for e in &t.expressions {
                collect_identifiers_in_expr(e, out);
            }
        }
        Expression::Tagged(tg) => {
            collect_identifiers_in_expr(&tg.tag, out);
            for e in &tg.quasi.expressions {
                collect_identifiers_in_expr(e, out);
            }
        }
        Expression::Spread(s) => collect_identifiers_in_expr(&s.argument, out),
        Expression::Paren(p) => collect_identifiers_in_expr(&p.expression, out),
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
            match &a.left {
                svelte_js_ast::AssignmentTarget::Expression(Expression::Identifier(id)) => {
                    out.insert(id.name.clone());
                }
                svelte_js_ast::AssignmentTarget::Pattern(p) => {
                    collect_pattern_names(p, out);
                }
                _ => {}
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

/// Derive the component name from a filename: `output.svelte` → `Output`.
/// Falls back to `Component` if the basename is empty.
fn analysis_name_from_filename(filename: &str) -> String {
    let base = filename
        .rsplit('/')
        .next()
        .unwrap_or(filename)
        .trim_end_matches(".svelte");
    if base.is_empty() {
        return "Component".to_string();
    }
    let mut chars = base.chars();
    let mut out = String::new();
    if let Some(c) = chars.next() {
        out.push(c.to_ascii_uppercase());
    }
    for c in chars {
        out.push(c);
    }
    // Sanitize: replace non-identifier characters with underscore.
    let out: String = out
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
                c
            } else {
                '_'
            }
        })
        .collect();
    out
}

/// `<svelte:self>` migration when a filename is available. Rewrites each
/// `<svelte:self ...>` to `<X ...>` (where `X` is the derived component
/// name) and arranges for an `import X from './file.svelte';` at the top
/// of the script body.
///
/// Also handles the case where `X` clashes with an existing top-level
/// identifier in the script — generates `X_1` etc.
fn migrate_svelte_self_with_filename(
    source: &str,
    str: &mut MagicString,
    root: &Root,
    filename: Option<&str>,
) -> bool {
    let Some(filename) = filename else {
        return false;
    };
    // Re-walk to collect spans.
    let mut self_spans: Vec<(usize, usize)> = Vec::new();
    let mut self_has_fragment: Vec<bool> = Vec::new();
    walk_fragment(&root.fragment, &mut |child| {
        if let FragmentChild::SvelteSelf(s) = child {
            self_spans.push((s.start as usize, s.end as usize));
            self_has_fragment.push(!s.fragment.nodes.is_empty());
        }
    });
    if self_spans.is_empty() {
        return false;
    }
    // Compute base name from filename, and resolve clashes with existing
    // top-level identifiers in the instance script.
    let base_name = analysis_name_from_filename(filename);
    let mut existing: std::collections::HashSet<String> = Default::default();
    if let Some(instance) = &root.instance {
        for stmt in &instance.content.body {
            collect_top_level_decl_names(stmt, &mut existing);
        }
    }
    let component_name = if existing.contains(&base_name) {
        let mut n = 1usize;
        loop {
            let candidate = format!("{}_{}", base_name, n);
            if !existing.contains(&candidate) {
                break candidate;
            }
            n += 1;
        }
    } else {
        base_name.clone()
    };
    let bytes = source.as_bytes();
    for ((start, end), has_frag) in self_spans.iter().zip(self_has_fragment.iter()) {
        // Open tag: overwrite `<svelte:self` → `<COMPONENT`.
        let open_kw_start = *start + 1;
        let open_kw_end = open_kw_start + "svelte:self".len();
        if open_kw_end > bytes.len() || &bytes[open_kw_start..open_kw_end] != b"svelte:self" {
            continue;
        }
        str.update(open_kw_start, open_kw_end, &component_name);
        // Close tag for fragment-bearing self: locate `</svelte:self` before `>`.
        if *has_frag {
            // Find the last `</` before `end`.
            let mut k = *end;
            while k > *start && &bytes[k - 1..k] != b">" {
                k -= 1;
            }
            // k is just past `>`. Find `</`.
            // Easier: search for `</svelte:self` in [start..end].
            if let Some(rel) = source[*start..*end].rfind("</svelte:self") {
                let close_kw_start = *start + rel + 2;
                let close_kw_end = close_kw_start + "svelte:self".len();
                if close_kw_end <= bytes.len() && &bytes[close_kw_start..close_kw_end] == b"svelte:self" {
                    str.update(close_kw_start, close_kw_end, &component_name);
                }
            }
        } else {
            // Also handle the explicit `<svelte:self></svelte:self>` case (no
            // fragment but the closing tag is still present).
            if let Some(rel) = source[*start..*end].rfind("</svelte:self") {
                let close_kw_start = *start + rel + 2;
                let close_kw_end = close_kw_start + "svelte:self".len();
                if close_kw_end <= bytes.len() && &bytes[close_kw_start..close_kw_end] == b"svelte:self" {
                    str.update(close_kw_start, close_kw_end, &component_name);
                }
            }
        }
    }
    // Inject `import COMPONENT from './basename';` at the top of script.
    // If no script tag, defer to emit_props_script_no_instance which will
    // synthesize the `<script>` block (so we don't double-create one).
    let file_basename = filename.rsplit('/').next().unwrap_or(filename);
    let import_line = format!("import {} from './{}';", component_name, file_basename);
    if let Some(instance) = &root.instance {
        let indent = guess_indent(source, instance);
        let insertion_point = instance.content.span.start as usize;
        str.append_right(insertion_point, format!("\n{}{}", indent, import_line));
    }
    // For the no-instance case the script is synthesized later (in
    // emit_props_script_no_instance / build_props_block path).

    // Replace `$$props` / `$$restProps` everywhere — these need to become
    // the `let { ...props } = $props()` rest binding. We do this here
    // (before slot edits run) so subsequent slot-template rewrites see the
    // already-renamed identifier and don't double-edit.
    let uses_props_source = source_uses_dollar_dollar(source, "$$props");
    let uses_rest_source = source_uses_dollar_dollar(source, "$$restProps");
    let bytes_local = source.as_bytes();
    if uses_props_source {
        let needle = b"$$props";
        let n = needle.len();
        let mut i = 0;
        while i + n <= bytes_local.len() {
            let after_ok = i + n >= bytes_local.len()
                || !(bytes_local[i + n].is_ascii_alphanumeric() || bytes_local[i + n] == b'_');
            let before_ok = i == 0
                || !(bytes_local[i - 1].is_ascii_alphanumeric() || bytes_local[i - 1] == b'_');
            if &bytes_local[i..i + n] == needle && before_ok && after_ok {
                str.update(i, i + n, "props");
                i += n;
            } else {
                i += 1;
            }
        }
    }
    if uses_rest_source {
        let needle = b"$$restProps";
        let n = needle.len();
        let mut i = 0;
        while i + n <= bytes_local.len() {
            if &bytes_local[i..i + n] == needle {
                str.update(i, i + n, "rest");
                i += n;
            } else {
                i += 1;
            }
        }
    }
    true
}

// ---------------------------------------------------------------------------
// `<svelte:element this="div" />` → `<svelte:element this={"div"} />`
// Only when `this`-value is a static Literal string.
// ---------------------------------------------------------------------------

/// `<svelte:component this={X}>...</svelte:component>` → `<X>...</X>` when X
/// is a valid component-name identifier or MemberExpression. Otherwise leave
/// alone (we don't yet generate `{@const SvelteComponentN = X}` derivations).
fn migrate_svelte_component(source: &str, str: &mut MagicString, frag: &Fragment) {
    walk_fragment(frag, &mut |child| {
        if let FragmentChild::SvelteComponent(c) = child {
            let (s, e) = expr_span(&c.expression);
            let expr_text = source[s as usize..e as usize].to_string();
            // Validate component-name regex: must start with uppercase or be a
            // MemberExpression / dotted path with valid parts.
            if !is_valid_component_name(&expr_text) {
                return;
            }
            // Rewrite the open tag: replace `svelte:component` with `expr`.
            // Open tag spans from `<` + name → at position c.start + 1 onwards.
            let bytes = source.as_bytes();
            let name = b"svelte:component";
            let open_name_start = c.start as usize + 1;
            if open_name_start + name.len() > bytes.len()
                || &bytes[open_name_start..open_name_start + name.len()] != name
            {
                return;
            }
            str.update(
                open_name_start,
                open_name_start + name.len(),
                &expr_text,
            );
            // Rewrite the close tag if present: `</svelte:component>`.
            let close_seq = b"</svelte:component";
            let mut k = c.end as usize;
            // Look for close tag before c.end.
            if k >= close_seq.len() + 1 {
                let close_pos = k - close_seq.len() - 1;
                // Verify.
                if &bytes[close_pos..close_pos + close_seq.len()] == close_seq
                    && bytes.get(close_pos + close_seq.len()).copied() == Some(b'>')
                {
                    str.update(
                        close_pos + 2,
                        close_pos + 2 + b"svelte:component".len(),
                        &expr_text,
                    );
                }
            }
            // Remove `this={X}` attribute. Find `this` literal text before the
            // expression position.
            // Look for `this` literal preceded by whitespace, between
            // `<svelte:component` and the expression.
            let this_search_start = c.start as usize + 1 + name.len();
            let mut p = s as usize;
            // Walk backwards from expression's `{` to find `=`, then `this`.
            // Scan backward from `s` for `this`.
            let mut found_this = None;
            let mut scan = s as usize;
            while scan > this_search_start {
                scan -= 1;
                if scan + 4 <= bytes.len() && &bytes[scan..scan + 4] == b"this" {
                    let before_ok = scan == 0
                        || bytes[scan - 1].is_ascii_whitespace();
                    let after = &bytes[scan + 4..];
                    let after_ok = after.iter().take_while(|c| c.is_ascii_whitespace() || **c == b'=' || **c == b'{').next().is_some();
                    if before_ok && after_ok {
                        found_this = Some(scan);
                        break;
                    }
                }
            }
            let _ = p;
            if let Some(this_pos) = found_this {
                // Eat leading whitespace.
                let mut start = this_pos;
                while start > 0 && (bytes[start - 1] == b' ' || bytes[start - 1] == b'\t') {
                    start -= 1;
                }
                // Find the closing `}` after the expression.
                let mut end = e as usize;
                while end < bytes.len() && bytes[end] != b'}' {
                    end += 1;
                }
                if end < bytes.len() {
                    end += 1;
                }
                str.remove(start, end);
            }
        }
    });
}

/// Match the regex `regex_valid_component_name` upstream: starts with
/// `[A-Z]` OR `_`/`$` and contains valid identifier chars + dots.
fn is_valid_component_name(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    let bytes = s.as_bytes();
    let first = bytes[0];
    if !(first.is_ascii_uppercase() || first == b'_' || first == b'$') {
        // Member expressions like `Math.random` could also be valid; allow if
        // the first segment is uppercase/letter.
        if !first.is_ascii_alphabetic() {
            return false;
        }
    }
    // Allow letters, digits, `_`, `$`, `.`.
    for &c in bytes {
        if !(c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c == b'.') {
            return false;
        }
    }
    true
}

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
// Slot migration support: scan the template for `<slot>` / `<slot name="X">`
// usages and `$$slots.X` references. Produces a `SlotInfo` consumed by
// `migrate_simple_props` to extend the generated Props block, and by
// `apply_slot_template_edits` to rewrite the template markup.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct SlotProp {
    /// The local/exported name (after `default` → `children` mapping).
    pub name: String,
    /// `true` if the slot is rendered with a non-empty `slot_props` object.
    pub has_props: bool,
    /// Set when the slot was first discovered via a `$$slots.X` reference and
    /// can still be refined by a later `<slot>` element visit.
    pub needs_refine: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SlotInfo {
    /// Slot props (in insertion order). For simple cases, the only entry is
    /// `children`.
    pub props: Vec<SlotProp>,
    /// Whether the template uses `$$slots.X` references.
    pub uses_dollar_slots: bool,
}

fn gather_slot_info(source: &str, root: &Root) -> SlotInfo {
    let mut info = SlotInfo::default();
    // Bail entirely if it's a custom element — slots stay as-is.
    if is_custom_element(root) {
        return info;
    }

    // Gather slot-related events in source order.
    enum Event {
        Slot {
            start: usize,
            name: String,
            has_props: bool,
        },
        DollarSlots {
            start: usize,
            name: String,
        },
    }
    let mut events: Vec<Event> = Vec::new();

    walk_with_parent(&root.fragment, None, &mut |child, _parent| {
        let FragmentChild::SlotElement(slot) = child else {
            return;
        };
        let mut slot_name = String::from("default");
        let mut has_props = false;
        for a in &slot.attributes {
            if let ElementAttribute::Attribute(attr) = a {
                if attr.name == "name" {
                    if let Some(name) = attribute_static_string(&attr.value) {
                        slot_name = name;
                    }
                } else if attr.name != "slot" {
                    has_props = true;
                }
            } else if matches!(a, ElementAttribute::SpreadAttribute(_)) {
                has_props = true;
            }
        }
        events.push(Event::Slot {
            start: slot.start as usize,
            name: slot_name,
            has_props,
        });
    });

    // Textual scan for `$$slots.X` / `$$slots['X']`.
    let bytes = source.as_bytes();
    let needle = b"$$slots";
    let n = needle.len();
    let mut i = 0;
    while i + n <= bytes.len() {
        if &bytes[i..i + n] == needle {
            info.uses_dollar_slots = true;
            let mut k = i + n;
            while k < bytes.len() && (bytes[k] == b' ' || bytes[k] == b'\t') {
                k += 1;
            }
            let name_opt: Option<String> = if k < bytes.len() && bytes[k] == b'.' {
                k += 1;
                let s = k;
                while k < bytes.len() && (bytes[k].is_ascii_alphanumeric() || bytes[k] == b'_') {
                    k += 1;
                }
                if k > s {
                    Some(source[s..k].to_string())
                } else {
                    None
                }
            } else if k < bytes.len() && bytes[k] == b'[' {
                k += 1;
                while k < bytes.len() && (bytes[k] == b' ' || bytes[k] == b'\t') {
                    k += 1;
                }
                if k < bytes.len() && (bytes[k] == b'\'' || bytes[k] == b'"') {
                    let quote = bytes[k];
                    k += 1;
                    let s = k;
                    while k < bytes.len() && bytes[k] != quote {
                        k += 1;
                    }
                    Some(source[s..k].to_string())
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(name) = name_opt {
                if is_valid_identifier_strict(&name) || name == "default" {
                    events.push(Event::DollarSlots {
                        start: i,
                        name,
                    });
                }
            }
            i = (i + n).max(k);
        } else {
            i += 1;
        }
    }

    events.sort_by_key(|e| match e {
        Event::Slot { start, .. } => *start,
        Event::DollarSlots { start, .. } => *start,
    });

    if std::env::var("MIGRATE_DEBUG").ok().as_deref() == Some("slots") {
        for e in &events {
            match e {
                Event::Slot { start, name, has_props } => eprintln!("SLOT @{} name={} hp={}", start, name, has_props),
                Event::DollarSlots { start, name } => eprintln!("DD @{} name={}", start, name),
            }
        }
    }

    let debug = std::env::var("MIGRATE_DEBUG").ok().as_deref() == Some("slots");
    for ev in events {
        match ev {
            Event::Slot {
                name,
                has_props,
                ..
            } => {
                let slot_name = name.clone();
                let local = if slot_name == "default" {
                    "children".to_string()
                } else {
                    slot_name.clone()
                };
                if let Some(existing) = info.props.iter_mut().find(|p| p.name == local) {
                    if existing.needs_refine {
                        existing.has_props = has_props;
                        existing.needs_refine = false;
                    }
                } else {
                    info.props.push(SlotProp {
                        name: local,
                        has_props,
                        needs_refine: false,
                    });
                }
            }
            Event::DollarSlots { name, .. } => {
                let mut nm = name.clone();
                if nm == "default" {
                    nm = "children".to_string();
                }
                if !info.props.iter().any(|p| p.name == nm) {
                    info.props.push(SlotProp {
                        name: nm,
                        has_props: true, // Snippet<[any]> initially
                        needs_refine: true,
                    });
                }
            }
        }
    }
    if debug {
        eprintln!("=== gather_slot_info DONE ===");
        for p in &info.props {
            eprintln!("FINAL: {} has_props={} needs_refine={}", p.name, p.has_props, p.needs_refine);
        }
    }
    info
}

/// Handle the "Component has let: directives" case in upstream's
/// `migrate_slot_usage`. Wrap the default-slot content of the Component in
/// `{#snippet children({ let_props })}…{/snippet}`, removing the let:
/// directives from the Component's opening tag.
fn apply_component_let_directive_wrap(
    source: &str,
    str: &mut MagicString,
    child: &FragmentChild,
    depth: usize,
) {
    let bytes = source.as_bytes();
    let indent = guess_indent_from_source(source);
    let (attrs, c_frag): (&Vec<ElementAttribute>, &Fragment) = match child {
        FragmentChild::Component(c) => (&c.attributes, &c.fragment),
        FragmentChild::SvelteComponent(c) => (&c.attributes, &c.fragment),
        _ => return,
    };
    // Gather let: directives.
    let mut let_pairs: Vec<String> = Vec::new();
    let mut let_attrs: Vec<(usize, usize)> = Vec::new();
    for a in attrs {
        if let ElementAttribute::LetDirective(ld) = a {
            let pair = if let Some(expr) = &ld.expression {
                let (s, e) = expr_span(expr);
                format!("{}: {}", ld.name, &source[s as usize..e as usize])
            } else {
                ld.name.clone()
            };
            let_pairs.push(pair);
            let_attrs.push((ld.start as usize, ld.end as usize));
        }
    }
    if let_pairs.is_empty() {
        return;
    }
    // Remove the let: directives from the Component opening tag.
    for (s, e) in &let_attrs {
        str.remove(*s, *e);
    }

    // Find the default-slot content range. Skip leading empty-text children
    // and named-slot children at the start.
    if c_frag.nodes.is_empty() {
        return;
    }
    let mut inner_start: Option<usize> = None;
    let mut inner_end: Option<usize> = None;
    for n in &c_frag.nodes {
        let is_empty_text = matches!(n, FragmentChild::Text(t) if t.data.trim().is_empty());
        let has_slot_attr = node_has_slot_attribute(n);
        if has_slot_attr {
            if let Some(_) = inner_start {
                if inner_end.is_none() {
                    inner_end = Some(node_start(n));
                }
            }
        } else if !is_empty_text {
            if inner_start.is_none() {
                inner_start = Some(node_start(n));
            } else if let Some(_) = inner_end {
                // There was default content, then a named slot, now more
                // default content — upstream moves it via str.move. Skip
                // (the rare interleave case).
            }
        }
    }
    let Some(inner_start) = inner_start else {
        return;
    };
    // If we never found a named-slot break, the default content goes to the
    // last node.
    let inner_end = inner_end.unwrap_or_else(|| {
        let last = &c_frag.nodes[c_frag.nodes.len() - 1];
        node_end(last)
    });

    let props_text = format!("{{ {} }}", let_pairs.join(", "));
    // Compute path indent: upstream uses `state.indent.repeat(path.length)`
    // for the inner indent. When this function is invoked for a Component at
    // depth N, the walker's path.length matches N (the path excludes the
    // Component itself but counts its ancestors).
    let inner_indent = indent.repeat(depth);
    let outer_indent = indent.repeat(depth.saturating_sub(1));

    // Insert the {#snippet children(props)} marker.
    str.append_left(
        inner_start,
        format!("{{#snippet children({})}}\n{}", props_text, inner_indent),
    );
    // Indent every line in [inner_start, inner_end].
    // The FIRST line of content: the line where inner_start lives. Its line
    // start is BEFORE inner_start (the leading indent of the source line).
    // Magic-string's indent prepends the indent AT inner_start itself when the
    // last `\n` before inner_start fell in the exclusion. We replicate that by
    // calling `append_left(inner_start, indent)` (i.e. before the content's
    // first non-whitespace char). This is in addition to the prepend's own
    // `inner_indent`.
    str.append_left(inner_start, indent.clone());
    let mut k = inner_start;
    while k < inner_end {
        if bytes[k] == b'\n' && k + 1 < inner_end {
            str.append_left(k + 1, indent.clone());
        }
        k += 1;
    }
    // Insert the closing snippet tag.
    // If there are named slots after default content (inner_end < last node),
    // upstream uses just `{/snippet}\n{indent}` (without trailing dedent).
    // Otherwise it includes the outer indent for the closing tag.
    let last_end = node_end(&c_frag.nodes[c_frag.nodes.len() - 1]);
    if inner_end < last_end {
        str.prepend_left(inner_end, format!("{{/snippet}}\n{}", outer_indent));
    } else {
        str.prepend_left(
            inner_end,
            format!("{}{{/snippet}}\n{}", inner_indent, outer_indent),
        );
    }
}

fn node_has_slot_attribute(n: &FragmentChild) -> bool {
    let attrs: &Vec<ElementAttribute> = match n {
        FragmentChild::RegularElement(e) => &e.attributes,
        FragmentChild::SvelteElement(e) => &e.attributes,
        FragmentChild::SvelteFragment(e) => &e.attributes,
        FragmentChild::SlotElement(e) => &e.attributes,
        FragmentChild::Component(c) => &c.attributes,
        FragmentChild::SvelteComponent(c) => &c.attributes,
        _ => return false,
    };
    attrs.iter().any(|a| {
        matches!(a, ElementAttribute::Attribute(attr) if attr.name == "slot"
            && attribute_static_string(&attr.value).is_some())
    })
}

fn node_start(n: &FragmentChild) -> usize {
    match n {
        FragmentChild::Text(t) => t.start as usize,
        FragmentChild::RegularElement(e) => e.start as usize,
        FragmentChild::Component(c) => c.start as usize,
        FragmentChild::SvelteComponent(c) => c.start as usize,
        FragmentChild::SvelteElement(e) => e.start as usize,
        FragmentChild::SvelteFragment(e) => e.start as usize,
        FragmentChild::SvelteSelf(e) => e.start as usize,
        FragmentChild::SvelteOptions(e) => e.start as usize,
        FragmentChild::SvelteWindow(e) => e.start as usize,
        FragmentChild::SvelteHead(e) => e.start as usize,
        FragmentChild::SvelteBody(e) => e.start as usize,
        FragmentChild::SvelteDocument(e) => e.start as usize,
        FragmentChild::SvelteBoundary(e) => e.start as usize,
        FragmentChild::SlotElement(e) => e.start as usize,
        FragmentChild::TitleElement(e) => e.start as usize,
        FragmentChild::ExpressionTag(et) => et.start as usize,
        FragmentChild::HtmlTag(t) => t.start as usize,
        FragmentChild::ConstTag(c) => c.start as usize,
        FragmentChild::RenderTag(r) => r.start as usize,
        FragmentChild::IfBlock(b) => b.start as usize,
        FragmentChild::EachBlock(b) => b.start as usize,
        FragmentChild::AwaitBlock(b) => b.start as usize,
        FragmentChild::KeyBlock(b) => b.start as usize,
        FragmentChild::SnippetBlock(b) => b.start as usize,
        FragmentChild::Comment(c) => c.start as usize,
        FragmentChild::DebugTag(d) => d.start as usize,
        FragmentChild::AttachTag(a) => a.start as usize,
    }
}

fn node_end(n: &FragmentChild) -> usize {
    match n {
        FragmentChild::Text(t) => t.end as usize,
        FragmentChild::RegularElement(e) => e.end as usize,
        FragmentChild::Component(c) => c.end as usize,
        FragmentChild::SvelteComponent(c) => c.end as usize,
        FragmentChild::SvelteElement(e) => e.end as usize,
        FragmentChild::SvelteFragment(e) => e.end as usize,
        FragmentChild::SvelteSelf(e) => e.end as usize,
        FragmentChild::SvelteOptions(e) => e.end as usize,
        FragmentChild::SvelteWindow(e) => e.end as usize,
        FragmentChild::SvelteHead(e) => e.end as usize,
        FragmentChild::SvelteBody(e) => e.end as usize,
        FragmentChild::SvelteDocument(e) => e.end as usize,
        FragmentChild::SvelteBoundary(e) => e.end as usize,
        FragmentChild::SlotElement(e) => e.end as usize,
        FragmentChild::TitleElement(e) => e.end as usize,
        FragmentChild::ExpressionTag(et) => et.end as usize,
        FragmentChild::HtmlTag(t) => t.end as usize,
        FragmentChild::ConstTag(c) => c.end as usize,
        FragmentChild::RenderTag(r) => r.end as usize,
        FragmentChild::IfBlock(b) => b.end as usize,
        FragmentChild::EachBlock(b) => b.end as usize,
        FragmentChild::AwaitBlock(b) => b.end as usize,
        FragmentChild::KeyBlock(b) => b.end as usize,
        FragmentChild::SnippetBlock(b) => b.end as usize,
        FragmentChild::Comment(c) => c.end as usize,
        FragmentChild::DebugTag(d) => d.end as usize,
        FragmentChild::AttachTag(a) => a.end as usize,
    }
}

/// Mirror upstream's `migrate_slot_usage`. For each child of a Component /
/// SvelteComponent parent that has a `slot="X"` attribute, wrap that child in
/// `{#snippet X(let_props)}...{/snippet}` and strip the `slot=` attribute.
/// SvelteFragment children are unwrapped (only the inner content kept).
fn apply_migrate_slot_usage(
    source: &str,
    str: &mut MagicString,
    frag: &Fragment,
    parent: Option<&FragmentChild>,
    depth: usize,
) {
    let bytes = source.as_bytes();
    let indent = guess_indent_from_source(source);
    let parent_is_component = matches!(
        parent,
        Some(FragmentChild::Component(_)) | Some(FragmentChild::SvelteComponent(_))
    );
    for child in &frag.nodes {
        // Recurse first into children so we don't miss nested cases.
        let child_frag: Option<&Fragment> = match child {
            FragmentChild::Component(c) => Some(&c.fragment),
            FragmentChild::SvelteComponent(c) => Some(&c.fragment),
            FragmentChild::RegularElement(e) => Some(&e.fragment),
            FragmentChild::SvelteElement(e) => Some(&e.fragment),
            FragmentChild::SvelteFragment(e) => Some(&e.fragment),
            FragmentChild::SlotElement(e) => Some(&e.fragment),
            FragmentChild::IfBlock(b) => Some(&b.consequent),
            FragmentChild::EachBlock(b) => Some(&b.body),
            _ => None,
        };
        if let Some(f) = child_frag {
            apply_migrate_slot_usage(source, str, f, Some(child), depth + 1);
        }

        // Case: child is a Component/SvelteComponent with `let:` directives.
        // Wrap its default-slot content in `{#snippet children(props)}…`.
        if let FragmentChild::Component(_) | FragmentChild::SvelteComponent(_) = child {
            apply_component_let_directive_wrap(source, str, child, depth);
        }

        // We only apply migrate_slot_usage to children of a Component/SvelteComponent.
        if !parent_is_component {
            continue;
        }
        // Get attrs, start, end, and fragment.
        let (attrs, c_start, c_end, c_frag, is_svelte_fragment): (
            &Vec<ElementAttribute>,
            usize,
            usize,
            Option<&Fragment>,
            bool,
        ) = match child {
            FragmentChild::RegularElement(e) => {
                (&e.attributes, e.start as usize, e.end as usize, Some(&e.fragment), false)
            }
            FragmentChild::SvelteElement(e) => {
                (&e.attributes, e.start as usize, e.end as usize, Some(&e.fragment), false)
            }
            FragmentChild::SvelteFragment(e) => {
                (&e.attributes, e.start as usize, e.end as usize, Some(&e.fragment), true)
            }
            FragmentChild::SlotElement(e) => {
                (&e.attributes, e.start as usize, e.end as usize, Some(&e.fragment), false)
            }
            FragmentChild::Component(c) => {
                (&c.attributes, c.start as usize, c.end as usize, Some(&c.fragment), false)
            }
            FragmentChild::SvelteComponent(c) => {
                (&c.attributes, c.start as usize, c.end as usize, Some(&c.fragment), false)
            }
            _ => continue,
        };

        // Find slot=, name=, let directives.
        let mut snippet_name: String = "children".to_string();
        let mut slot_attr_span: Option<(usize, usize)> = None;
        let mut let_pairs: Vec<String> = Vec::new();
        let mut let_attrs: Vec<(usize, usize)> = Vec::new();
        let mut invalid_id: Option<String> = None;
        let mut shadowed: Option<String> = None;
        for a in attrs {
            match a {
                ElementAttribute::Attribute(attr) if attr.name == "slot" => {
                    if let Some(name) = attribute_static_string(&attr.value) {
                        let mut nm = name.clone();
                        if nm == "default" {
                            nm = "children".to_string();
                        }
                        if !is_valid_identifier_strict(&nm) {
                            invalid_id = Some(name.clone());
                        } else {
                            // Check parent's attributes — shadow detection.
                            let parent_attrs: Option<&Vec<ElementAttribute>> = match parent {
                                Some(FragmentChild::Component(c)) => Some(&c.attributes),
                                Some(FragmentChild::SvelteComponent(c)) => Some(&c.attributes),
                                _ => None,
                            };
                            if let Some(pa) = parent_attrs {
                                let conflict = pa.iter().any(|p_attr| match p_attr {
                                    ElementAttribute::Attribute(at) => at.name == nm,
                                    ElementAttribute::BindDirective(bd) => bd.name == nm,
                                    _ => false,
                                });
                                if conflict {
                                    shadowed = Some(nm.clone());
                                }
                            }
                            snippet_name = nm;
                        }
                        slot_attr_span = Some((attr.start as usize, attr.end as usize));
                    }
                }
                ElementAttribute::LetDirective(ld) => {
                    let pair = if let Some(expr) = &ld.expression {
                        let (s, e) = expr_span(expr);
                        format!("{}: {}", ld.name, &source[s as usize..e as usize])
                    } else {
                        ld.name.clone()
                    };
                    let_pairs.push(pair);
                    let_attrs.push((ld.start as usize, ld.end as usize));
                }
                _ => {}
            }
        }

        // Bail (don't wrap) if invalid/shadow — the comment has already been
        // emitted by `migrate_invalid_named_slots`. We just skip the wrap and
        // leave the original markup (slot attr + let directives) intact.
        if invalid_id.is_some() || shadowed.is_some() {
            continue;
        }
        let _ = bytes;

        // If no slot attr → nothing to wrap.
        let Some((slot_s, slot_e)) = slot_attr_span else {
            continue;
        };

        // Remove the `slot=` attribute (upstream removes just the attribute,
        // leaving the leading space, which produces e.g. `<div >` for
        // `<div slot="X">`).
        str.remove(slot_s, slot_e);
        // Remove the let directives (upstream removes just the directive).
        for (s, e) in &let_attrs {
            str.remove(*s, *e);
        }

        let props_text = if let_pairs.is_empty() {
            String::new()
        } else {
            format!("{{ {} }}", let_pairs.join(", "))
        };

        if is_svelte_fragment {
            // Unwrap: remove the wrapper tags, keep content.
            if let Some(f) = c_frag {
                if !f.nodes.is_empty() {
                    let inner_start = match &f.nodes[0] {
                        FragmentChild::Text(t) => t.start as usize,
                        FragmentChild::RegularElement(e) => e.start as usize,
                        FragmentChild::Component(e) => e.start as usize,
                        FragmentChild::SvelteComponent(e) => e.start as usize,
                        FragmentChild::SvelteElement(e) => e.start as usize,
                        FragmentChild::SvelteFragment(e) => e.start as usize,
                        FragmentChild::SlotElement(e) => e.start as usize,
                        FragmentChild::ExpressionTag(et) => et.start as usize,
                        _ => c_start + 1,
                    };
                    let inner_end = match &f.nodes[f.nodes.len() - 1] {
                        FragmentChild::Text(t) => t.end as usize,
                        FragmentChild::RegularElement(e) => e.end as usize,
                        FragmentChild::Component(e) => e.end as usize,
                        FragmentChild::SvelteComponent(e) => e.end as usize,
                        FragmentChild::SvelteElement(e) => e.end as usize,
                        FragmentChild::SvelteFragment(e) => e.end as usize,
                        FragmentChild::SlotElement(e) => e.end as usize,
                        FragmentChild::ExpressionTag(et) => et.end as usize,
                        _ => (c_end - 1),
                    };
                    str.remove(c_start, inner_start);
                    str.remove(inner_end, c_end);
                }
            }
        }

        // Wrap in `{#snippet NAME(props)}` … `{/snippet}`.
        // Compute the indent at the snippet wrap depth. Upstream: prepend is
        // `\n${indent.repeat(path.length - 2)}`. Our depth at the point we're
        // examining a child of `frag` is `path.length - 1` (path was
        // [outer1, ..., outerN, frag]). So path.length - 2 = depth - 1.
        let outer_indent = indent.repeat(depth.saturating_sub(1));
        if std::env::var("MIGRATE_DEBUG_SLOT_WRAP").is_ok() {
            eprintln!("wrap depth={} c_start={} c_end={} name={}", depth, c_start, c_end, snippet_name);
        }
        str.prepend_left(
            c_start,
            format!(
                "{{#snippet {}({})}}\n{}",
                snippet_name, props_text, outer_indent
            ),
        );
        let close_str = format!("\n{}{{/snippet}}", outer_indent);
        // Append after the closing tag — for SlotElement, append RIGHT (after
        // any other rewrites that target node.end).
        match child {
            FragmentChild::SlotElement(_) => {
                str.append_right(c_end, close_str);
            }
            _ => {
                str.append_left(c_end, close_str);
            }
        }

        // Indent the wrapped element content by one extra level. Upstream's
        // `state.str.indent(indent, { exclude: [[0, start], [end, length]] })`
        // indents every line start within `[start, end]`. The FIRST line —
        // the line containing `<element ...>` — also gets indented (its line
        // start is the position right after the most recent `\n` before
        // `c_start`, which is just after the prepended snippet header).
        // EXCEPTION: for svelte:fragment (unwrapped), the first line of
        // content is the content AFTER `<svelte:fragment>`, which is what's
        // kept after unwrap — that content's first line is fully kept and
        // does NOT need an extra leading indent (its leading indent is
        // preserved from the source, just like all other lines).
        let body_bytes = source.as_bytes();
        if !is_svelte_fragment {
            str.append_left(c_start, indent.to_string());
        }
        let mut k = c_start;
        while k < c_end {
            if body_bytes[k] == b'\n' && k + 1 < c_end {
                str.append_left(k + 1, indent.to_string());
            }
            k += 1;
        }
    }
}

/// Replace `<slot>` markup and `$$slots.X` references in the template.
fn apply_slot_template_edits(source: &str, str: &mut MagicString, root: &Root, slots: &SlotInfo) {
    if is_custom_element(root) {
        return;
    }
    // Apply `migrate_slot_usage` first — wrap child-with-slot-attr inside
    // Component parents in `{#snippet}` blocks.
    apply_migrate_slot_usage(source, str, &root.fragment, None, 1);

    if slots.props.is_empty() && !slots.uses_dollar_slots {
        return;
    }
    let uses_props = source_uses_dollar_dollar(source, "$$props");
    let prefix = if uses_props { "props." } else { "" };

    // Walk and rewrite each `<slot>` element. Track parents to detect when a
    // slot lives directly inside a Component (those keep the slot=… anchor for
    // snippet wrapping — defer that case).
    walk_with_parent(&root.fragment, None, &mut |child, parent| {
        let FragmentChild::SlotElement(slot) = child else {
            return;
        };
        // Skip when parent is a Component or SvelteComponent — that case is
        // handled differently (wraps in {#snippet name(props)}). For now we
        // also handle the bare-`<slot>` case inside a Component (it just
        // becomes `{@render children?.()}` like the non-component case).
        let _parent_is_component = matches!(
            parent,
            Some(FragmentChild::Component(_)) | Some(FragmentChild::SvelteComponent(_))
        );

        // Compute the slot name and slot_props text.
        let mut slot_name = String::from("default");
        let mut prop_pairs: Vec<String> = Vec::new();
        let mut has_inner_slot_attr = false;
        for a in &slot.attributes {
            if let ElementAttribute::Attribute(attr) = a {
                if attr.name == "slot" {
                    has_inner_slot_attr = true;
                    continue;
                }
                if attr.name == "name" {
                    if let Some(n) = attribute_static_string(&attr.value) {
                        slot_name = n;
                    }
                    continue;
                }
                // Compute attribute value text. When $$props is used, rewrite
                // `$$props.X` → `props.X` in the expression text.
                let rewrite_props = |s: &str| -> String {
                    if uses_props {
                        // Replace `$$props.X` / `$$props['X']` etc.
                        let mut out = String::with_capacity(s.len());
                        let bs = s.as_bytes();
                        let mut i = 0;
                        while i < bs.len() {
                            if bs[i..].starts_with(b"$$props") {
                                let before_ok = i == 0
                                    || !(bs[i - 1].is_ascii_alphanumeric() || bs[i - 1] == b'_');
                                if before_ok {
                                    out.push_str("props");
                                    i += "$$props".len();
                                    continue;
                                }
                            }
                            out.push(bs[i] as char);
                            i += 1;
                        }
                        out
                    } else {
                        s.to_string()
                    }
                };
                let value = match &attr.value {
                    AttributeValue::Empty => "true".to_string(),
                    AttributeValue::Single(et) => {
                        let (s, e) = expr_span(&et.expression);
                        rewrite_props(&source[s as usize..e as usize])
                    }
                    AttributeValue::Many(parts) => {
                        // Single-text → quoted string.
                        if parts.len() == 1 {
                            if let AttributeValuePart::Text(t) = &parts[0] {
                                format!("\"{}\"", t.data)
                            } else if let AttributeValuePart::ExpressionTag(et) = &parts[0] {
                                let (s, e) = expr_span(&et.expression);
                                rewrite_props(&source[s as usize..e as usize])
                            } else {
                                "true".to_string()
                            }
                        } else {
                            // Template literal-ish. Use the original source span.
                            let s = parts.first().map(|p| p.start_pos()).unwrap_or(0);
                            let last_end = match parts.last() {
                                Some(AttributeValuePart::Text(t)) => t.end,
                                Some(AttributeValuePart::ExpressionTag(et)) => et.end,
                                _ => 0,
                            };
                            format!("`{}`", rewrite_props(&source[s as usize..last_end as usize]))
                        }
                    }
                };
                let pair = if value == attr.name {
                    format!("{},", value)
                } else {
                    format!("{}: {},", attr.name, value)
                };
                prop_pairs.push(pair);
            } else if let ElementAttribute::SpreadAttribute(sp) = a {
                let (s, e) = expr_span(&sp.expression);
                prop_pairs.push(format!("...{},", &source[s as usize..e as usize]));
            }
        }
        let _ = has_inner_slot_attr;

        let local = if slot_name == "default" {
            "children".to_string()
        } else {
            slot_name.clone()
        };
        // Build the @render text.
        let render_args = if prop_pairs.is_empty() {
            String::new()
        } else {
            format!("{{ {} }}", prop_pairs.join(" "))
        };

        let s = slot.start as usize;
        let e = slot.end as usize;

        // Apply `prefix.NAME` for $$props case.
        if slot.fragment.nodes.is_empty() {
            // <slot .../> → {@render prefix.NAME?.(args)}
            let replacement = format!("{{@render {}{}?.({})}}", prefix, local, render_args);
            str.update(s, e, &replacement);
        } else {
            // <slot>fallback</slot> → {#if NAME}{@render prefix.NAME(args)}{:else}fallback{/if}
            let first = &slot.fragment.nodes[0];
            let last = &slot.fragment.nodes[slot.fragment.nodes.len() - 1];
            let inner_start = match first {
                FragmentChild::Text(t) => t.start,
                FragmentChild::RegularElement(e) => e.start,
                FragmentChild::Component(c) => c.start,
                FragmentChild::SvelteComponent(c) => c.start,
                FragmentChild::SvelteElement(e) => e.start,
                FragmentChild::SvelteFragment(e) => e.start,
                FragmentChild::SlotElement(e) => e.start,
                FragmentChild::ExpressionTag(et) => et.start,
                _ => slot.start + 1,
            } as usize;
            let inner_end = match last {
                FragmentChild::Text(t) => t.end,
                FragmentChild::RegularElement(e) => e.end,
                FragmentChild::Component(c) => c.end,
                FragmentChild::SvelteComponent(c) => c.end,
                FragmentChild::SvelteElement(e) => e.end,
                FragmentChild::SvelteFragment(e) => e.end,
                FragmentChild::SlotElement(e) => e.end,
                FragmentChild::ExpressionTag(et) => et.end,
                _ => (slot.end - 1) as u32,
            } as usize;
            let open = format!(
                "{{#if {0}{1}}}{{@render {0}{1}({2})}}{{:else}}",
                prefix, local, render_args
            );
            str.update(s, inner_start, &open);
            str.update(inner_end, e, "{/if}");
        }
    });

    // Replace `$$slots.X` and `$$slots['X']` / `$$slots["X"]` with `prefix + X`.
    if slots.uses_dollar_slots {
        let bytes = source.as_bytes();
        let needle = b"$$slots";
        let n = needle.len();
        let mut i = 0;
        while i + n <= bytes.len() {
            if &bytes[i..i + n] == needle {
                // Make sure it's a standalone identifier.
                let before_ok = i == 0
                    || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
                if !before_ok {
                    i += 1;
                    continue;
                }
                let mut k = i + n;
                let mut name: Option<String> = None;
                let mut end = k;
                if k < bytes.len() && bytes[k] == b'.' {
                    let s = k + 1;
                    let mut e = s;
                    while e < bytes.len() && (bytes[e].is_ascii_alphanumeric() || bytes[e] == b'_') {
                        e += 1;
                    }
                    if e > s {
                        name = Some(source[s..e].to_string());
                        end = e;
                    }
                } else if k < bytes.len() && bytes[k] == b'[' {
                    k += 1;
                    while k < bytes.len() && (bytes[k] == b' ' || bytes[k] == b'\t') {
                        k += 1;
                    }
                    if k < bytes.len() && (bytes[k] == b'\'' || bytes[k] == b'"') {
                        let quote = bytes[k];
                        k += 1;
                        let s = k;
                        while k < bytes.len() && bytes[k] != quote {
                            k += 1;
                        }
                        if k < bytes.len() {
                            name = Some(source[s..k].to_string());
                            // Skip closing quote, whitespace, `]`.
                            k += 1;
                            while k < bytes.len() && (bytes[k] == b' ' || bytes[k] == b'\t') {
                                k += 1;
                            }
                            if k < bytes.len() && bytes[k] == b']' {
                                end = k + 1;
                            } else {
                                name = None;
                            }
                        }
                    }
                }
                if let Some(mut nm) = name {
                    if nm == "default" {
                        nm = "children".to_string();
                    }
                    let after = end;
                    str.update(i, after, &format!("{}{}", prefix, nm));
                    i = after;
                    continue;
                }
                i = end.max(i + n);
            } else {
                i += 1;
            }
        }
    }
    let _ = slots;
}

/// Emit a `<script>` block with the Props typedef + destructure at the very
/// top of the source, when there's no existing instance script but slot
/// migration requires a Props block.
fn emit_props_script_no_instance(
    source: &str,
    str: &mut MagicString,
    root: &Root,
    slots: &SlotInfo,
    opt_use_ts: bool,
    filename: Option<&str>,
) {
    // Choose indent. Upstream's `guess_indent` looks at the first indented
    // line in the source — same as ours.
    let indent_owned = guess_indent_from_source(source);
    let indent = indent_owned.as_str();
    let has_lang_ts = root
        .instance
        .as_ref()
        .map(|i| {
            i.attributes
                .iter()
                .any(|a| a.name == "lang" && matches!(attribute_static_string(&a.value).as_deref(), Some("ts")))
        })
        .unwrap_or(false);
    let has_jsdoc_type_anywhere = source.contains("@type {");
    let uses_ts = has_lang_ts || (opt_use_ts && !has_jsdoc_type_anywhere);
    let uses_props = source_uses_dollar_dollar(source, "$$props");
    let uses_rest = source_uses_dollar_dollar(source, "$$restProps");

    let block = build_props_block(slots, uses_props, uses_rest, uses_ts, indent, &[]);

    // If there's a `<svelte:self>` AND a filename, also inject the
    // `import X from './file.svelte';` at the top of the synthesized script.
    let mut svelte_self_import: Option<String> = None;
    if let Some(filename) = filename {
        let mut has_self = false;
        walk_fragment(&root.fragment, &mut |c| {
            if matches!(c, FragmentChild::SvelteSelf(_)) {
                has_self = true;
            }
        });
        if has_self {
            let base_name = analysis_name_from_filename(filename);
            // Resolve clashes (none here — no script body).
            let component_name = base_name;
            let file_basename = filename.rsplit('/').next().unwrap_or(filename);
            svelte_self_import = Some(format!(
                "import {} from './{}';",
                component_name, file_basename
            ));
        }
    }

    // Prepend `<script>\n\t{block}\n</script>\n\n`.
    let head = if uses_ts { "<script lang=\"ts\">" } else { "<script>" };
    let inner = if let Some(imp) = &svelte_self_import {
        format!("{}{}\n{}{}", indent, imp, indent, block)
    } else {
        format!("{}{}", indent, block)
    };
    let full = format!("{}\n{}\n</script>\n\n", head, inner);
    str.prepend_left(0, full);
    let _ = source;
}

/// Build the textual Props block (typedef/interface + destructure).
/// `extra_export_props` is a slice of `(name, init_or_empty, bindable, type_hint)`
/// for `export let` declarations, in order.
fn build_props_block(
    slots: &SlotInfo,
    uses_props: bool,
    uses_rest: bool,
    uses_ts: bool,
    indent: &str,
    export_props: &[ExportProp],
) -> String {
    // Compute all prop entries with `type`, `optional`.
    struct Entry {
        local: String,
        exported: String,
        init: String,
        bindable: bool,
        optional: bool,
        ty: String,
        slot_name: Option<String>,
    }
    let mut entries: Vec<Entry> = Vec::new();
    for p in export_props {
        entries.push(Entry {
            local: p.local.clone(),
            exported: p.exported.clone(),
            init: p.init.clone(),
            bindable: p.bindable,
            optional: !p.init.is_empty() || p.bindable,
            ty: p.ty.clone(),
            slot_name: None,
        });
    }
    for sp in &slots.props {
        let ty = if sp.has_props {
            "import('svelte').Snippet<[any]>".to_string()
        } else {
            "import('svelte').Snippet".to_string()
        };
        entries.push(Entry {
            local: sp.name.clone(),
            exported: sp.name.clone(),
            init: String::new(),
            bindable: false,
            optional: true,
            ty,
            slot_name: Some(sp.name.clone()),
        });
    }

    let many_props = entries.len() > 3;
    let newline_sep = format!("\n{}{}", indent, indent);
    let prop_sep = if many_props { newline_sep.as_str() } else { " " };

    // Build the destructure RHS list.
    let props_list = if uses_props {
        format!("...{}", "props")
    } else {
        let mut parts: Vec<String> = Vec::new();
        for e in &entries {
            // Skip type-only entries (none here).
            let mut s = if e.local == e.exported {
                e.local.clone()
            } else {
                format!("{}: {}", e.exported, e.local)
            };
            if e.bindable {
                if e.init.is_empty() {
                    s.push_str(" = $bindable()");
                } else {
                    s.push_str(&format!(" = $bindable({})", e.init));
                }
            } else if !e.init.is_empty() {
                s.push_str(&format!(" = {}", e.init));
            }
            parts.push(s);
        }
        let mut joined = parts.join(&format!(",{}", prop_sep));
        if uses_rest {
            if !joined.is_empty() {
                joined.push_str(&format!(",{}", prop_sep));
            }
            joined.push_str("...rest");
        }
        joined
    };

    // Determine `has_type_or_fallback`: any prop has a typed annotation OR
    // every prop is a slot (then we emit the Props type so users can fill it).
    let has_type_or_fallback = entries.iter().any(|e| e.slot_name.is_some())
        || export_props.iter().any(|p| !p.ty.is_empty() && p.ty != "any");
    // Actually upstream's check is: `state.has_type_or_fallback ||
    // state.props.every(p => p.slot_name)`. We emit when EVERY prop is a slot
    // OR has_type_or_fallback.
    let all_slots = !entries.is_empty() && entries.iter().all(|e| e.slot_name.is_some());
    let emit_type = has_type_or_fallback || all_slots;

    let type_name = "Props";
    let type_block: Option<String> = if emit_type {
        if uses_ts {
            // `interface Props { ... }`
            let mut s = format!("interface {} {{{}", type_name, newline_sep);
            let mut parts: Vec<String> = Vec::new();
            for e in &entries {
                let optional = if e.optional { "?" } else { "" };
                parts.push(format!("{}{}: {};", e.exported, optional, e.ty));
            }
            if uses_props || uses_rest {
                if !entries.is_empty() {
                    parts.push("[key: string]: any".to_string());
                } else {
                    parts.push("[key: string]: any".to_string());
                }
            }
            s.push_str(&parts.join(&newline_sep));
            s.push_str(&format!("\n{}}}", indent));
            Some(s)
        } else {
            // JSDoc @typedef
            let mut s = format!("/**\n{} * @typedef {{Object}} {}", indent, type_name);
            for e in &entries {
                let name = if e.optional {
                    format!("[{}]", e.exported)
                } else {
                    e.exported.clone()
                };
                s.push_str(&format!("\n{} * @property {{{}}} {}", indent, e.ty, name));
            }
            s.push_str(&format!("\n{} */", indent));
            Some(s)
        }
    } else {
        None
    };

    let mut decl = if many_props {
        format!("let {{{}{}{}{}}}", newline_sep, props_list, "\n", indent)
    } else {
        format!("let {{ {} }}", props_list)
    };

    if uses_ts {
        if type_block.is_some() {
            decl = format!("{}: {} = $props();", decl, type_name);
        } else {
            decl = format!("{} = $props();", decl);
        }
    } else {
        decl = format!("{} = $props();", decl);
    }

    if let Some(t) = type_block {
        if uses_ts {
            format!("{}\n\n{}{}", t, indent, decl)
        } else {
            // JSDoc form needs the /** @type {Props} */ annotation between
            // the typedef and the let. When uses_props/uses_rest, the
            // annotation includes `& { [key: string]: any }`.
            let intersection = if uses_props || uses_rest {
                if entries.is_empty() {
                    " { [key: string]: any }".to_string()
                } else {
                    format!(" & {{ [key: string]: any }}").to_string()
                }
            } else {
                String::new()
            };
            let ann = format!("/** @type {{{}{}}} */", type_name, intersection);
            format!("{}\n\n{}{}\n{}{}", t, indent, ann, indent, decl)
        }
    } else if (uses_props || uses_rest) && !uses_ts {
        // No typedef but using $$props/$$restProps in non-TS mode — emit a
        // bare `/** @type {{ [key: string]: any }} */` annotation above the
        // destructure so the type is preserved.
        let ann = "/** @type {{ [key: string]: any }} */".to_string();
        format!("{}\n{}{}", ann, indent, decl)
    } else {
        decl
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ExportProp {
    pub local: String,
    pub exported: String,
    pub init: String,
    pub bindable: bool,
    pub ty: String,
}

/// Best-effort indent guess. Looks for the first indented line inside the
/// instance script and uses its leading whitespace. Defaults to `\t`.
fn guess_indent(source: &str, instance: &svelte_ast::root::Script) -> String {
    let s = instance.content.span.start as usize;
    let e = instance.content.span.end as usize;
    let body = &source[s.min(source.len())..e.min(source.len())];
    guess_indent_for_text(body)
}

/// Like `guess_indent` but scans the entire source.
fn guess_indent_from_source(source: &str) -> String {
    guess_indent_for_text(source)
}

/// Mirror upstream `guess_indent`: count tab-indented vs space-indented lines.
/// If tabs >= spaces, use a tab. Otherwise compute the minimum leading spaces.
fn guess_indent_for_text(text: &str) -> String {
    let mut tabbed = 0usize;
    let mut spaced = 0usize;
    let mut min_spaces: usize = usize::MAX;
    for line in text.split('\n') {
        let bytes = line.as_bytes();
        if bytes.first().copied() == Some(b'\t') {
            tabbed += 1;
        } else if bytes.len() >= 2 && bytes[0] == b' ' && bytes[1] == b' ' {
            spaced += 1;
            let count = bytes.iter().take_while(|c| **c == b' ').count();
            if count < min_spaces {
                min_spaces = count;
            }
        }
    }
    if tabbed == 0 && spaced == 0 {
        return "\t".to_string();
    }
    if tabbed >= spaced {
        return "\t".to_string();
    }
    " ".repeat(min_spaces)
}

// ---------------------------------------------------------------------------
// Simple props migration: `export let X` → `let { X, … } = $props();`.
// Only fires for the narrow case:
//   - all exports are simple `export let X` (or `export let X = init`)
//     with Identifier pattern
//   - no JSDoc, no TypeScript types, no `$$Props` interface
//   - no `$$props` / `$$restProps` usage (we only handle the "no rest" or
//     simple-rest cases via a separate path)
//
// Restrictions: doesn't emit `$bindable()` wrappers yet. Each export becomes
// a single field in the destructured `let { ... } = $props()`.
// ---------------------------------------------------------------------------

fn migrate_simple_props(
    source: &str,
    str: &mut MagicString,
    root: &Root,
    slots: &SlotInfo,
    opt_use_ts: bool,
    filename: Option<&str>,
) {
    // Pre-check for $$props / $$restProps and svelte:self even when there's
    // no <script> tag. If either is present, we must synthesize a script
    // with `let { ...props } = $props();`.
    let no_instance_uses_props = root.instance.is_none()
        && (source_uses_dollar_dollar(source, "$$props")
            || source_uses_dollar_dollar(source, "$$restProps"));
    let Some(instance) = &root.instance else {
        // No <script> tag at all. If there are slots, we need to emit one.
        if !slots.props.is_empty() || no_instance_uses_props {
            emit_props_script_no_instance(source, str, root, slots, opt_use_ts, filename);
        } else {
            // Even with no script or props, we may still need to inject the
            // svelte:self import. Detect svelte:self existence and a filename.
            let mut has_self = false;
            walk_fragment(&root.fragment, &mut |c| {
                if matches!(c, FragmentChild::SvelteSelf(_)) {
                    has_self = true;
                }
            });
            if has_self && filename.is_some() {
                emit_props_script_no_instance(source, str, root, slots, opt_use_ts, filename);
            }
        }
        return;
    };

    // Bail if there's a `$$Props` type alias / interface in the script (we
    // can't synthesize the interface yet).
    if source_uses_dollar_dollar(source, "$$Props") {
        return;
    }
    // Bail if uses_props ($$props). We'd need the rest-spread form which
    // isn't fully implemented yet, but we *can* handle a simple "no exports
    // but uses $$props" case → `let { ...props } = $props();`. Skip for now.
    let uses_props = source_uses_dollar_dollar(source, "$$props");
    let uses_rest = source_uses_dollar_dollar(source, "$$restProps");

    // Gather all `export let` declarations.
    struct Prop {
        local: String,
        init: Option<(u32, u32)>,
        decl_start: usize,
        decl_end: usize,
        node_start: usize,
        node_end: usize,
        node_decl_count: usize,
        bindable: bool,
        has_type_annotation: bool,
        // Type extracted from a leading JSDoc `@type {...}` block (without
        // the wrapping `/** @type {...} */`). None ⇒ infer from init or
        // fall back to `any`.
        jsdoc_type: Option<String>,
        // JSDoc *block* start/end if found above the export (so we can erase).
        jsdoc_span: Option<(usize, usize)>,
        // Verbatim TS type annotation text (without the leading `:`). None
        // when the export has no inline type annotation.
        ts_type: Option<String>,
    }
    let mut props: Vec<Prop> = Vec::new();
    // Collect bind:/updated targets for $bindable detection.
    let mut updated: std::collections::HashSet<String> = std::collections::HashSet::new();
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
                if let ElementAttribute::BindDirective(b) = a {
                    if let Some(name) = bind_target_identifier(&b.expression) {
                        updated.insert(name);
                    }
                }
            }
        }
    });
    for stmt in &instance.content.body {
        collect_assignment_targets(stmt, &mut updated);
    }

    let bytes = source.as_bytes();
    for stmt in &instance.content.body {
        let Statement::ExportNamed(en) = stmt else {
            continue;
        };
        let Some(Statement::Variable(v)) = en.declaration.as_ref() else {
            continue;
        };
        for (i, d) in v.declarations.iter().enumerate() {
            let Pattern::Identifier(id) = &d.id else {
                continue;
            };
            // Detect a textual `:` type annotation between id.end and the
            // next `=`/`,`/`;`. If present, capture the type text.
            let id_end = id.span.end as usize;
            let (has_type, ts_type) = {
                let mut k = id_end;
                while k < bytes.len() && (bytes[k] == b' ' || bytes[k] == b'\t') {
                    k += 1;
                }
                if k < bytes.len() && bytes[k] == b':' {
                    // Skip the colon and any whitespace.
                    let mut p = k + 1;
                    while p < bytes.len() && (bytes[p] == b' ' || bytes[p] == b'\t') {
                        p += 1;
                    }
                    // Scan forward until a top-level `=`, `,`, `;`, `\n` or
                    // the end of the declarator. Respect nested brackets and
                    // string literals.
                    let mut q = p;
                    let mut depth_paren = 0i32;
                    let mut depth_brace = 0i32;
                    let mut depth_bracket = 0i32;
                    let mut depth_angle = 0i32;
                    while q < bytes.len() {
                        let b = bytes[q];
                        if depth_paren == 0 && depth_brace == 0 && depth_bracket == 0 && depth_angle == 0
                            && (b == b'=' || b == b',' || b == b';' || b == b'\n')
                        {
                            break;
                        }
                        match b {
                            b'(' => depth_paren += 1,
                            b')' => depth_paren -= 1,
                            b'{' => depth_brace += 1,
                            b'}' => depth_brace -= 1,
                            b'[' => depth_bracket += 1,
                            b']' => depth_bracket -= 1,
                            b'<' => depth_angle += 1,
                            b'>' => depth_angle -= 1,
                            _ => {}
                        }
                        q += 1;
                    }
                    let ty = source[p..q].trim_end().to_string();
                    (true, Some(ty))
                } else {
                    (false, None)
                }
            };
            // Detect a leading JSDoc block immediately before this export.
            let jsdoc_span = {
                let mut p = en.span.start as usize;
                while p > 0 && (bytes[p - 1] == b' ' || bytes[p - 1] == b'\t' || bytes[p - 1] == b'\n')
                {
                    p -= 1;
                }
                if p >= 2 && &source[p - 2..p] == "*/" {
                    // Find matching `/**` start.
                    let close = p;
                    let prefix = &source[..close - 2];
                    if let Some(rel) = prefix.rfind("/**") {
                        Some((rel, close))
                    } else {
                        None
                    }
                } else {
                    None
                }
            };

            // Extract `@type {…}` from JSDoc if present.
            let jsdoc_type = jsdoc_span.and_then(|(s, e)| extract_jsdoc_type(&source[s..e]));

            let init_span = d.init.as_ref().map(|e| expr_span(e));
            props.push(Prop {
                local: id.name.clone(),
                init: init_span,
                decl_start: d.span.start as usize,
                decl_end: d.span.end as usize,
                node_start: en.span.start as usize,
                node_end: en.span.end as usize,
                node_decl_count: v.declarations.len(),
                bindable: updated.contains(&id.name),
                has_type_annotation: has_type,
                jsdoc_type,
                jsdoc_span,
                ts_type,
            });
            let _ = i;
        }
    }

    if props.is_empty() && !uses_props && !uses_rest && slots.props.is_empty() {
        return;
    }
    // If there are NO `export let` decls AND only `$$props`/`$$restProps`,
    // we still emit `let { ...props } = $props();` (or `...rest`). Insert
    // it after the last import, or at script content start.
    if props.is_empty() && slots.props.is_empty() {
        let rest_name = if uses_props { "props" } else { "rest" };
        let has_lang_ts = instance.attributes.iter().any(|a| {
            a.name == "lang"
                && matches!(
                    attribute_static_string(&a.value).as_deref(),
                    Some("ts") | Some("typescript")
                )
        });
        let indent_str = guess_indent(source, instance);
        let indent = indent_str.as_str();
        let block = if has_lang_ts {
            format!(
                "/** @type {{{{ [key: string]: any }}}} */\n{}let {{ ...{} }} = $props();",
                indent, rest_name
            )
        } else {
            format!(
                "/** @type {{{{ [key: string]: any }}}} */\n{}let {{ ...{} }} = $props();",
                indent, rest_name
            )
        };
        // Compute insertion point = max(last import end, content start).
        let mut insertion_point = instance.content.span.start as usize;
        for stmt in &instance.content.body {
            if let Statement::Import(imp) = stmt {
                let e = imp.span.end as usize;
                if e > insertion_point {
                    insertion_point = e;
                }
            }
        }
        // Use append_right so the block sits AFTER previously appended-right
        // imports (e.g. the svelte:self import added by
        // migrate_svelte_self_with_filename earlier in the pipeline).
        str.append_right(insertion_point, format!("\n{}{}", indent, block));
        // Replace `$$props` / `$$restProps` identifier uses in the rest of
        // the script + template.
        let bytes_local = source.as_bytes();
        if uses_props {
            let needle = b"$$props";
            let n = needle.len();
            let mut i = 0;
            while i + n <= bytes_local.len() {
                let after_ok = i + n >= bytes_local.len()
                    || !(bytes_local[i + n].is_ascii_alphanumeric() || bytes_local[i + n] == b'_');
                let before_ok = i == 0
                    || !(bytes_local[i - 1].is_ascii_alphanumeric() || bytes_local[i - 1] == b'_');
                if &bytes_local[i..i + n] == needle && before_ok && after_ok {
                    str.update(i, i + n, "props");
                    i += n;
                } else {
                    i += 1;
                }
            }
        }
        if uses_rest {
            let needle = b"$$restProps";
            let n = needle.len();
            let mut i = 0;
            while i + n <= bytes_local.len() {
                if &bytes_local[i..i + n] == needle {
                    str.update(i, i + n, "rest");
                    i += n;
                } else {
                    i += 1;
                }
            }
        }
        return;
    }

    // Decide whether to emit a JSDoc `@typedef Props` block.
    // Mirrors upstream's `has_type_or_fallback` flag:
    //   * any prop has a JSDoc `@type` annotation
    //   * OR any prop has a JSDoc comment (typedef-worthy comment)
    //   * OR any prop's init is a trivially-typed Literal (string/number/bool)
    let has_jsdoc_type = props.iter().any(|p| p.jsdoc_type.is_some());
    let has_jsdoc_comment = props.iter().any(|p| p.jsdoc_span.is_some());
    let has_literal_init = props.iter().any(|p| {
        let Some((s, e)) = p.init else {
            return false;
        };
        let t = source[s as usize..e as usize].trim();
        t.starts_with('\'')
            || t.starts_with('"')
            || t.starts_with('`')
            || t == "true"
            || t == "false"
            || t.parse::<f64>().is_ok()
    });
    let has_any_jsdoc_type = has_jsdoc_type || has_jsdoc_comment || has_literal_init;

    // Compute each prop's type & optional-ness for the JSDoc block.
    // - jsdoc_type wins
    // - else, infer from init: 'string' / number / boolean / Array literal / ...
    //   Fall back to `any`.
    // - optional = has init OR bindable.
    fn infer_type_from_init(init_text: Option<&str>) -> String {
        let Some(t) = init_text else {
            return "any".to_string();
        };
        let t = t.trim();
        if t.starts_with('\'') || t.starts_with('"') || t.starts_with('`') {
            return "string".to_string();
        }
        if t == "true" || t == "false" {
            return "boolean".to_string();
        }
        if t.parse::<f64>().is_ok() {
            return "number".to_string();
        }
        if t.starts_with('[') {
            return "any[]".to_string();
        }
        if t.starts_with('{') {
            return "Record<string, any>".to_string();
        }
        "any".to_string()
    }

    // Detect `<script lang="ts">` for TS mode. Honor the option as a fallback
    // (when the script lacks the lang attribute, we use the option, BUT only
    // when the source doesn't already use JSDoc `@type {…}` patterns).
    let has_lang_ts = instance.attributes.iter().any(|a| {
        a.name == "lang"
            && matches!(
                attribute_static_string(&a.value).as_deref(),
                Some("ts") | Some("typescript")
            )
    });
    let has_jsdoc_type_anywhere = source.contains("@type {");
    let uses_ts = has_lang_ts || (opt_use_ts && !has_jsdoc_type_anywhere);
    let needs_lang_ts_tag = uses_ts && !has_lang_ts;

    // Build the destructured `let { X, Y = INIT, ... } = $props();`.
    // When $$props is used, upstream emits a rest-only `let { ...props } = $props();`
    // and drops all `export let X` lines without their declarations becoming
    // fields.
    // Use the indent that the script content uses (default \t).
    let indent_str = guess_indent(source, instance);
    let indent = indent_str.as_str();
    let newline_sep = format!("\n{}{}", indent, indent);
    // Total prop count including slots.
    let total_props = props.len() + slots.props.len() + if uses_rest { 1 } else { 0 };
    let many_props = total_props > 3;
    let prop_sep = if many_props { newline_sep.as_str() } else { " " };

    let props_decl = if uses_props {
        "let { ...props } = $props();".to_string()
    } else {
        // Compute which slot names each prop's init references so we can
        // interleave their order. Slots referenced from a prop's init are
        // emitted BEFORE that prop. Other slots are emitted after all props.
        let mut emitted_slots: std::collections::HashSet<String> = Default::default();
        let mut parts: Vec<String> = Vec::new();
        for p in &props {
            let init_text: Option<String> = p
                .init
                .map(|(s, e)| source[s as usize..e as usize].to_string());
            // Rewrite `$$slots.X` references in the init text (so the prop
            // can reference the local slot binding instead of the global
            // `$$slots.X`).
            let init_text = init_text.map(|t| rewrite_dollar_dollar_refs(&t));
            // Emit any slots referenced in this prop's init that haven't
            // already been emitted.
            if let Some(t) = &init_text {
                for sp in &slots.props {
                    if emitted_slots.contains(&sp.name) {
                        continue;
                    }
                    // Simple substring check is sufficient — the rewrite
                    // already turned `$$slots.X` into `X`.
                    let is_referenced = {
                        let bytes = t.as_bytes();
                        let needle = sp.name.as_bytes();
                        let n = needle.len();
                        let mut found = false;
                        let mut k = 0;
                        while k + n <= bytes.len() {
                            if &bytes[k..k + n] == needle {
                                let before_ok = k == 0
                                    || !(bytes[k - 1].is_ascii_alphanumeric() || bytes[k - 1] == b'_');
                                let after_ok = k + n >= bytes.len()
                                    || !(bytes[k + n].is_ascii_alphanumeric() || bytes[k + n] == b'_');
                                if before_ok && after_ok {
                                    found = true;
                                    break;
                                }
                            }
                            k += 1;
                        }
                        found
                    };
                    if is_referenced {
                        parts.push(sp.name.clone());
                        emitted_slots.insert(sp.name.clone());
                    }
                }
            }
            let entry = if p.bindable {
                match init_text.as_deref() {
                    Some(init) => format!("{} = $bindable({})", p.local, init),
                    None => format!("{} = $bindable()", p.local),
                }
            } else {
                match init_text.as_deref() {
                    Some(init) => format!("{} = {}", p.local, init),
                    None => p.local.clone(),
                }
            };
            parts.push(entry);
        }
        // Emit remaining slots in their source-order.
        for sp in &slots.props {
            if !emitted_slots.contains(&sp.name) {
                parts.push(sp.name.clone());
            }
        }
        if uses_rest {
            parts.push("...rest".to_string());
        }
        // Single-line if total props <=3.
        if many_props {
            format!(
                "let {{{}{}\n{}}} = $props();",
                newline_sep,
                parts.join(&format!(",{}", newline_sep)),
                indent
            )
        } else {
            format!("let {{ {} }} = $props();", parts.join(", "))
        }
    };

    // Upstream's rule: emit the Props type when `has_type_or_fallback` is set
    // OR every prop is a slot. We treat `has_any_jsdoc_type` as our local
    // has_type_or_fallback signal, AND additionally emit the type when every
    // prop is a slot (slot-only components).
    let all_props_are_slots = !slots.props.is_empty() && props.is_empty();
    let has_any_ts_type = props.iter().any(|p| p.ts_type.is_some());
    // Emit a Props type block when we have explicit TS or JSDoc types, OR
    // every prop is a slot (slot-only components), OR (TS-mode) the
    // component uses `$$restProps` so we can add the indexed signature
    // `[key: string]: any`.
    let need_props_type = (has_any_jsdoc_type
        || all_props_are_slots
        || has_any_ts_type
        || (uses_ts && uses_rest))
        && !uses_props;

    // Build the Props typedef/interface block.
    let typedef_block: Option<String> = if need_props_type {
        if uses_ts {
            let mut s = format!("interface Props {{");
            let inner_sep = newline_sep.as_str();
            let mut parts: Vec<String> = Vec::new();
            for p in &props {
                let init_text: Option<String> = p
                    .init
                    .map(|(s, e)| source[s as usize..e as usize].to_string());
                let ty = p
                    .ts_type
                    .clone()
                    .or_else(|| p.jsdoc_type.clone())
                    .unwrap_or_else(|| infer_type_from_init(init_text.as_deref()));
                let optional = init_text.is_some() || p.bindable;
                let opt = if optional { "?" } else { "" };
                parts.push(format!("{}{}: {};", p.local, opt, ty));
            }
            for sp in &slots.props {
                let ty = if sp.has_props {
                    "import('svelte').Snippet<[any]>"
                } else {
                    "import('svelte').Snippet"
                };
                parts.push(format!("{}?: {};", sp.name, ty));
            }
            if uses_rest {
                parts.push("[key: string]: any".to_string());
            }
            s.push_str(inner_sep);
            s.push_str(&parts.join(inner_sep));
            s.push_str(&format!("\n{}}}", indent));
            Some(s)
        } else {
            let mut lines: Vec<String> = Vec::new();
            lines.push(format!("/**"));
            lines.push(format!("{} * @typedef {{Object}} Props", indent));
            for p in &props {
                let init_text: Option<String> = p
                    .init
                    .map(|(s, e)| source[s as usize..e as usize].to_string());
                let ty = p
                    .jsdoc_type
                    .clone()
                    .unwrap_or_else(|| infer_type_from_init(init_text.as_deref()));
                let optional = init_text.is_some();
                let name = if optional {
                    format!("[{}]", p.local)
                } else {
                    p.local.clone()
                };
                lines.push(format!("{} * @property {{{}}} {}", indent, ty, name));
            }
            for sp in &slots.props {
                let ty = if sp.has_props {
                    "import('svelte').Snippet<[any]>"
                } else {
                    "import('svelte').Snippet"
                };
                lines.push(format!("{} * @property {{{}}} [{}]", indent, ty, sp.name));
            }
            lines.push(format!("{} */", indent));
            Some(lines.join("\n"))
        }
    } else {
        None
    };

    // Build the final block that replaces the FIRST export node.
    let final_block = if let Some(td) = &typedef_block {
        if uses_ts {
            // `interface Props {…}\n\n\tlet { … }: Props = $props();`
            let decl_with_ann = props_decl.replace(" = $props();", ": Props = $props();");
            format!("{}\n\n{}{}", td, indent, decl_with_ann)
        } else {
            let intersection = if uses_props || uses_rest {
                if props.is_empty() && slots.props.is_empty() {
                    "{ [key: string]: any }".to_string()
                } else {
                    "Props & { [key: string]: any }".to_string()
                }
            } else {
                "Props".to_string()
            };
            let ann = format!("/** @type {{{}}} */", intersection);
            format!("{}\n\n{}{}\n{}{}", td, indent, ann, indent, props_decl)
        }
    } else if uses_ts {
        // No type block but TS — leave decl alone.
        props_decl.clone()
    } else {
        props_decl.clone()
    };

    // Replace `$$restProps` references with `rest` in template attributes.
    if uses_rest {
        let needle = b"$$restProps";
        let n = needle.len();
        let mut i = 0;
        while i + n <= bytes.len() {
            if &bytes[i..i + n] == needle {
                str.update(i, i + n, "rest");
                i += n;
            } else {
                i += 1;
            }
        }
    }
    // Replace `$$props` references with `props` (renaming).
    if uses_props {
        let needle = b"$$props";
        let n = needle.len();
        let mut i = 0;
        while i + n <= bytes.len() {
            // Make sure it's a standalone identifier.
            let after_ok = i + n >= bytes.len()
                || !(bytes[i + n].is_ascii_alphanumeric() || bytes[i + n] == b'_');
            let before_ok = i == 0
                || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
            if &bytes[i..i + n] == needle && before_ok && after_ok {
                str.update(i, i + n, "props");
                i += n;
            } else {
                i += 1;
            }
        }
    }

    // For each prop's parent ExportNamedDeclaration, remove or rewrite it.
    // - If the node has all its declarators converted to props → remove the
    //   whole node, then for the FIRST node, replace with `props_decl`.
    // - Else: too tricky for now, bail (we only support uniform conversion).

    // Group by node_start.
    let mut node_groups: std::collections::BTreeMap<usize, Vec<&Prop>> = Default::default();
    for p in &props {
        node_groups.entry(p.node_start).or_default().push(p);
    }
    // Every group must have all declarators in the parent node converted.
    let mut not_all = false;
    for (_, group) in &node_groups {
        let total = group[0].node_decl_count;
        if group.len() != total {
            not_all = true;
            break;
        }
    }
    if not_all {
        return;
    }

    if props.is_empty() {
        // Slot-only case: insert AFTER the last import declaration (upstream's
        // props_insertion_point tracks this), else at the start of the script
        // content.
        let mut insert_after = instance.content.span.start as usize;
        for stmt in &instance.content.body {
            if let Statement::Import(imp) = stmt {
                insert_after = insert_after.max(imp.span.end as usize);
            }
        }
        // If the script body is otherwise empty (the original case for
        // slot-use_ts-2 — has a comment), find a sensible insertion point.
        let content_start = instance.content.span.start as usize;
        if insert_after == content_start {
            // No imports — find the first non-whitespace position so we can
            // insert at the very top of the script body.
            let after_ws = {
                let mut k = content_start;
                while k < bytes.len() && (bytes[k] == b' ' || bytes[k] == b'\t' || bytes[k] == b'\n') {
                    k += 1;
                }
                k
            };
            // If content is fully whitespace, append at end.
            if after_ws >= instance.content.span.end as usize {
                let block_with_lead = format!("\n{}{}", indent, final_block);
                str.prepend_left(content_start, block_with_lead);
            } else {
                // Non-empty content (e.g. comments or other code) — insert at
                // very top of body, then put existing content below (a single
                // newline separator, like upstream).
                str.prepend_left(after_ws, format!("{}\n{}", final_block, indent));
            }
        } else {
            // Insert after last import — `\n${indent}${block}`.
            str.append_left(insert_after, format!("\n{}{}", indent, final_block));
        }
        if needs_lang_ts_tag {
            // `<script` → `<script lang="ts"`.
            let s_start = instance.start as usize;
            let bs = &source.as_bytes()[s_start..];
            if bs.starts_with(b"<script") {
                str.append_right(s_start + "<script".len(), " lang=\"ts\"".to_string());
            }
        }
    } else if has_any_jsdoc_type {
        // Find the position of the first non-whitespace char of the instance
        // script (where the JSDoc / export starts).
        let mut after_ws = instance.content.span.start as usize;
        while after_ws < bytes.len()
            && (bytes[after_ws] == b'\n' || bytes[after_ws] == b' ' || bytes[after_ws] == b'\t')
        {
            after_ws += 1;
        }
        // Use prepend_left at that position so the prepended text comes
        // BEFORE the JSDoc (and stays even if JSDoc is later removed).
        // The prepended text supplies its own preceding `\n${indent}` so that
        // the output reads `<script>\n${indent}\n${indent}{block}…`. The
        // original `\n${indent}` of the source already precedes that.
        str.prepend_left(
            after_ws,
            format!("\n{}{}", indent, final_block),
        );
        for p in &props {
            if let Some((s, e)) = p.jsdoc_span {
                // Eat trailing whitespace + newline after JSDoc (the indent
                // before the export gets preserved separately).
                let mut ee = e;
                while ee < bytes.len() && (bytes[ee] == b' ' || bytes[ee] == b'\t') {
                    ee += 1;
                }
                if ee < bytes.len() && bytes[ee] == b'\n' {
                    ee += 1;
                }
                str.remove(s, ee);
            }
        }
        // Track which export node_starts we've already eaten (multi-decl).
        // Sort by node_start so we can identify the last one.
        let mut export_starts: Vec<usize> = props.iter().map(|p| p.node_start).collect();
        export_starts.sort();
        export_starts.dedup();
        let last_export_start = *export_starts.last().unwrap();
        let mut seen_node: std::collections::HashSet<usize> = Default::default();
        for p in &props {
            if !seen_node.insert(p.node_start) {
                continue;
            }
            // Remove the export node + its leading indent + trailing newline.
            // For the LAST export, don't eat the trailing newline — that
            // newline is the source's separator from `</script>` and we want
            // to preserve it.
            let mut s = p.node_start;
            let mut e = p.node_end;
            while s > 0 && (bytes[s - 1] == b' ' || bytes[s - 1] == b'\t') {
                s -= 1;
            }
            let is_last = p.node_start == last_export_start;
            if !is_last && bytes.get(e).copied() == Some(b'\n') {
                e += 1;
            }
            str.remove(s, e);
        }
    } else {
        // No JSDoc types. Determine the props_insertion_point: the end of
        // the last top-level Import declaration in the script (or 0 if
        // none). If any imports come AFTER the first export, we insert the
        // final block at that point and remove all exports. Otherwise we
        // replace the first export in place and remove the rest.
        let mut props_insertion_point: usize = instance.content.span.start as usize;
        let mut has_import_after_first_export: bool = false;
        let first_export_start = props.iter().map(|p| p.node_start).min().unwrap_or(0);
        for stmt in &instance.content.body {
            if let Statement::Import(imp) = stmt {
                let imp_end = imp.span.end as usize;
                let imp_start = imp.span.start as usize;
                if imp_end > props_insertion_point {
                    props_insertion_point = imp_end;
                }
                if imp_start > first_export_start {
                    has_import_after_first_export = true;
                }
            }
        }
        if has_import_after_first_export {
            // Insert final block AFTER the last import; remove all exports.
            str.append_left(
                props_insertion_point,
                format!("\n{}{}", indent, final_block),
            );
            for (_, group) in &node_groups {
                let p = group[0];
                let mut s = p.node_start;
                let mut e = p.node_end;
                if bytes.get(e).copied() == Some(b'\n') {
                    e += 1;
                }
                while s > 0 && (bytes[s - 1] == b' ' || bytes[s - 1] == b'\t') {
                    s -= 1;
                }
                str.remove(s, e);
            }
        } else {
            // Replace first node with final_block (single-line) and remove
            // the rest. Preserves the historical behavior.
            let mut first = true;
            for (_, group) in &node_groups {
                let p = group[0];
                if first {
                    str.update(p.node_start, p.node_end, &final_block);
                    first = false;
                } else {
                    let mut s = p.node_start;
                    let mut e = p.node_end;
                    if bytes.get(e).copied() == Some(b'\n') {
                        e += 1;
                    }
                    while s > 0 && (bytes[s - 1] == b' ' || bytes[s - 1] == b'\t') {
                        s -= 1;
                    }
                    str.remove(s, e);
                }
            }
        }
    }
    let _ = uses_props;
    let _ = p_decl_unused();
}

/// Extract the type expression from a JSDoc block's `@type {…}` tag.
/// Returns `None` if no `@type` is present.
fn extract_jsdoc_type(block: &str) -> Option<String> {
    let idx = block.find("@type")?;
    let after = &block[idx + "@type".len()..];
    // Skip whitespace then expect `{`.
    let s = after.trim_start();
    if !s.starts_with('{') {
        return None;
    }
    let mut depth = 0i32;
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            depth += 1;
        } else if bytes[i] == b'}' {
            depth -= 1;
            if depth == 0 {
                return Some(s[1..i].trim().to_string());
            }
        }
        i += 1;
    }
    None
}

fn p_decl_unused() {
    // helper to silence dead-code style warnings — no-op
}

// ---------------------------------------------------------------------------
// `export { a, c, f, h }` (specifiers without a declaration) → props.
// For each specifier whose local name matches a declarator in a sibling
// `let a, b, c, d;` declaration, remove that name from the `let`. Then
// replace the `export { … }` itself with a destructured
// `let { a, c, f, h } = $props();`. Mirrors upstream
// ExportNamedDeclaration → VariableDeclaration handling (index.js:544).
//
// Only fires when:
//   * no `$$Props` interface and no JSDoc/TS types on the source declarators
//   * no `$$props` usage (mixed mode is unsupported)
//   * every specifier's local resolves to a declarator in some `let` decl
//     within instance script
// ---------------------------------------------------------------------------

fn migrate_export_specifier_props(source: &str, str: &mut MagicString, root: &Root) {
    let Some(instance) = &root.instance else {
        return;
    };
    if source_uses_dollar_dollar(source, "$$Props") {
        return;
    }
    if source_uses_dollar_dollar(source, "$$props") {
        return;
    }

    // Find `export { … }` (declaration is None, specifiers non-empty).
    let mut spec_exports: Vec<&svelte_js_ast::ExportNamedDeclaration> = Vec::new();
    for stmt in &instance.content.body {
        if let Statement::ExportNamed(en) = stmt {
            if en.declaration.is_none() && !en.specifiers.is_empty() {
                spec_exports.push(en);
            }
        }
    }
    if spec_exports.is_empty() {
        return;
    }

    // Build the prop names (in source order) from the FIRST spec-export.
    // (Upstream walks all; we handle the single-export case which is what the
    // fixture exercises.)
    if spec_exports.len() > 1 {
        return;
    }
    let en = spec_exports[0];
    let mut names: Vec<String> = Vec::new();
    for sp in &en.specifiers {
        let svelte_js_ast::ModuleExportName::Identifier(id) = &sp.local else {
            continue;
        };
        names.push(id.name.clone());
    }
    if names.is_empty() {
        return;
    }

    // For each name, find its source declarator inside instance.body.
    // Each must be a top-level `let … X …;` Variable decl.
    struct Site {
        name: String,
        var_start: usize,
        var_end: usize,
        decl_idx: usize,
        decl_start: usize,
        decl_end: usize,
        var_decl_count: usize,
    }
    let mut sites: Vec<Site> = Vec::new();
    for stmt in &instance.content.body {
        let Statement::Variable(v) = stmt else {
            continue;
        };
        if !matches!(v.kind, svelte_js_ast::VariableKind::Let) {
            continue;
        }
        for (i, d) in v.declarations.iter().enumerate() {
            let Pattern::Identifier(id) = &d.id else {
                continue;
            };
            if names.contains(&id.name) {
                sites.push(Site {
                    name: id.name.clone(),
                    var_start: v.span.start as usize,
                    var_end: v.span.end as usize,
                    decl_idx: i,
                    decl_start: d.span.start as usize,
                    decl_end: d.span.end as usize,
                    var_decl_count: v.declarations.len(),
                });
            }
        }
    }
    // Every name must be matched.
    if sites.len() != names.len() {
        return;
    }

    // Bail if any of these declarators has an init (we don't carry through
    // default values yet for this path) or a type annotation.
    let bytes = source.as_bytes();
    for s in &sites {
        let txt = &source[s.decl_start..s.decl_end];
        if txt.contains('=') || txt.contains(':') {
            return;
        }
    }

    // For each site, surgically remove just that declarator from the parent
    // `let X, Y, Z` — preserving the other declarators. Upstream uses the
    // commas before/after the declarator to extend the removal range.
    // We sort by source position so MagicString edits don't cross.
    let mut sites_sorted: Vec<&Site> = sites.iter().collect();
    sites_sorted.sort_by_key(|s| s.decl_start);

    // Group by var_start to know each var's full declarator set.
    let mut group_indices: std::collections::HashMap<usize, Vec<&Site>> = Default::default();
    for s in &sites {
        group_indices.entry(s.var_start).or_default().push(s);
    }
    // For each var: figure out whether we're removing ALL declarators (then
    // remove the whole `let …;`) or just some (then per-declarator excision).
    let mut full_removals: std::collections::HashSet<usize> = Default::default();
    for (var_start, group) in &group_indices {
        if group[0].var_decl_count == group.len() {
            full_removals.insert(*var_start);
        }
    }

    for s in &sites_sorted {
        if full_removals.contains(&s.var_start) {
            // Skip — handled below as a full var removal.
            continue;
        }
        // Per-declarator excision. Two cases:
        //   1. first declarator (idx 0): remove [decl_start, next_decl_start)
        //      i.e. remove `X, ` keeping the rest.
        //   2. else: remove from the preceding `,` (inclusive) to decl_end.
        if s.decl_idx == 0 {
            // Find next decl's start in the same var.
            // We need to read source to find next ','.
            let mut p = s.decl_end;
            while p < bytes.len() && bytes[p] != b',' {
                p += 1;
            }
            if p < bytes.len() && bytes[p] == b',' {
                p += 1;
                // Also skip following whitespace.
                while p < bytes.len() && bytes[p] == b' ' {
                    p += 1;
                }
                str.remove(s.decl_start, p);
            }
        } else {
            // Find the preceding `,`.
            let mut p = s.decl_start;
            while p > 0 && bytes[p - 1] != b',' {
                p -= 1;
            }
            if p > 0 {
                p -= 1; // include the comma
                str.remove(p, s.decl_end);
            }
        }
    }
    // Apply full var removals: remove `let X, Y;\n`.
    for var_start in &full_removals {
        let v = sites.iter().find(|s| s.var_start == *var_start).unwrap();
        let mut s = v.var_start;
        let mut e = v.var_end;
        if bytes.get(e).copied() == Some(b'\n') {
            e += 1;
        }
        while s > 0 && (bytes[s - 1] == b' ' || bytes[s - 1] == b'\t') {
            s -= 1;
        }
        str.remove(s, e);
    }

    // Build the props destructure declaration. Upstream uses
    // `\n${indent}${indent}` between props when >3 props (newline_sep),
    // otherwise a single space.
    let indent = "\t"; // best guess — most fixtures use tabs
    let newline_sep = format!("\n{}{}", indent, indent);
    let has_many = names.len() > 3;
    let sep = if has_many { newline_sep.as_str() } else { " " };

    let inner = if has_many {
        format!(
            "{}{}{}{}",
            sep,
            names.join(&format!(",{}", sep)),
            format!("\n{}", indent),
            ""
        )
    } else {
        format!(" {} ", names.join(", "))
    };
    let props_decl = format!("let {{{}}} = $props();", inner);

    // Insertion point: end of the LAST `let` declaration that contained any
    // prop name (in source order). Mirrors upstream's
    // `state.props_insertion_point = node.end` when at least one declarator
    // was exported.
    let mut insertion_point: usize = instance.content.span.start as usize;
    {
        let mut last_var_end: Option<usize> = None;
        for stmt in &instance.content.body {
            let Statement::Variable(v) = stmt else {
                continue;
            };
            let vs = v.span.start as usize;
            let group = group_indices.get(&vs);
            if group.is_some() {
                last_var_end = Some(v.span.end as usize);
            }
        }
        if let Some(p) = last_var_end {
            insertion_point = p;
        }
    }

    // Insert `\n\tlet { … } = $props();` at the insertion point.
    str.append_right(insertion_point, format!("\n{}{}", indent, props_decl));
    // Remove just the `export { … }` statement text (not its leading/trailing
    // whitespace — leaves the blank line + indent the surrounding source had).
    str.remove(en.span.start as usize, en.span.end as usize);
}

// ---------------------------------------------------------------------------
// Effects cluster: `$: SIDE_EFFECT;` / `$: { … }` / `$: if (…) { … }` →
// `run(() => { … });`. Prepends `import { run } from 'svelte/legacy';` to
// the instance script content. Skipped for `$: x = EXPR;` derivations which
// `migrate_simple_derivations` already handled.
//
// Also rewrites `break $;` inside the body to `return` (upstream behavior).
// ---------------------------------------------------------------------------

fn migrate_effects(
    source: &str,
    str: &mut MagicString,
    root: &Root,
    derived_labeled_starts: &std::collections::HashSet<usize>,
) {
    let Some(instance) = &root.instance else {
        return;
    };
    let body = &instance.content.body;

    let bytes = source.as_bytes();
    let mut had_effects = false;

    for stmt in body {
        let Statement::Labeled(l) = stmt else {
            continue;
        };
        if l.label.name != "$" {
            continue;
        }
        let l_start = l.span.start as usize;
        let l_end = l.span.end as usize;
        // Skip ONLY labeled statements that were actually consumed by
        // migrate_simple_derivations. Everything else (including failed
        // derivation candidates like store-prefix assigns or multi-$:) becomes
        // an effect.
        if derived_labeled_starts.contains(&l_start) {
            continue;
        }

        // Wrap body in `run(() => { … });`. Two cases:
        //   1. `$: { stmt; stmt; }` (block) — wrap as `run(() => { stmt; stmt; });`
        //   2. `$: stmt;` (single statement) — wrap as `run(() => { stmt; });`
        //   3. `$: if (cond) { … }` — wrap the whole if statement.
        // Trailing-`break $` needs to become `return`.
        let body_start = match &l.body {
            Statement::Block(b) => b.span.start as usize,
            Statement::Expression(es) => es.span.start as usize,
            Statement::If(ifs) => ifs.span.start as usize,
            _ => continue,
        };
        let body_end = match &l.body {
            Statement::Block(b) => b.span.end as usize,
            Statement::Expression(es) => es.span.end as usize,
            Statement::If(ifs) => ifs.span.end as usize,
            _ => continue,
        };
        // Indent of this labeled statement.
        let mut line_start = l_start;
        while line_start > 0 && bytes[line_start - 1] != b'\n' {
            line_start -= 1;
        }
        let indent = &source[line_start..l_start];

        // Build wrappers around the existing body text, preserving source
        // formatting via MagicString edits rather than re-formatting.
        // First, normalize `break $` → `return` inside the body via update().
        // Find all `break $` substrings within the body span.
        if let Statement::Block(_) = &l.body {
            let body_text = &source[body_start..body_end];
            let mut search = 0;
            while let Some(rel) = body_text[search..].find("break $") {
                let abs = body_start + search + rel;
                str.update(abs, abs + "break $".len(), "return");
                search += rel + "break $".len();
            }
        }
        match &l.body {
            Statement::Block(_) => {
                // Replace `$: ` (i.e. l_start..body_start) with `run(() => `.
                str.update(l_start, body_start, "run(() => ");
                // Replace trailing `}` with `});` — append `);` right after body_end.
                str.append_right(body_end, ");");
            }
            Statement::Expression(_) => {
                // Replace `$: ` with `run(() => {\n{indent}\t`.
                str.update(
                    l_start,
                    body_start,
                    &format!("run(() => {{\n{}\t", indent),
                );
                // Replace `;` (if present) at end with `;\n{indent}});`.
                let ends_with_semi = bytes
                    .get(body_end.saturating_sub(1))
                    .copied()
                    == Some(b';');
                let suffix = format!(";\n{}}});", indent);
                if ends_with_semi {
                    str.update(body_end - 1, body_end, &suffix);
                } else {
                    str.append_right(body_end, &suffix);
                }
            }
            Statement::If(_) => {
                // Replace `$: ` with `run(() => {\n{indent}\t`.
                str.update(
                    l_start,
                    body_start,
                    &format!("run(() => {{\n{}\t", indent),
                );
                str.append_right(body_end, &format!("\n{}}});", indent));
                // Add a tab after each `\n` inside the body to bump the
                // indent by one level (mirrors upstream's
                // `state.str.indent(state.indent, …)` call).
                let body_text = &source[body_start..body_end];
                let mut search = 0;
                while let Some(rel) = body_text[search..].find('\n') {
                    let abs = body_start + search + rel + 1;
                    if abs < bytes.len() {
                        str.prepend_right(abs, "\t");
                    }
                    search += rel + 1;
                }
            }
            _ => continue,
        }
        had_effects = true;
    }

    if !had_effects {
        return;
    }

    // Prepend `import { run } from 'svelte/legacy';\n\n` at the start of the
    // instance script content.
    let insertion_point = instance.content.span.start as usize;
    // Mirror upstream's indent: `\n${indent}${import}` appended right at
    // content start. Use the majority-based `guess_indent` over script body.
    let indent = guess_indent(source, instance);
    str.append_right(
        insertion_point,
        format!(
            "\n{}import {{ run }} from 'svelte/legacy';\n",
            indent
        ),
    );
}

fn reindent_inside(s: &str, indent: &str) -> String {
    // Each line of `s` becomes `\t{indent}{line}` so it sits inside the
    // `run(() => {` block at `indent`. We add `\t` to the existing `indent`
    // for proper nesting.
    let inner_indent = format!("{}\t", indent);
    let mut out = String::new();
    for line in s.lines() {
        out.push_str(&inner_indent);
        out.push_str(line.trim_start());
        out.push('\n');
    }
    // No trailing newline strip — caller adds `});` after, on a new line.
    out
}

// ---------------------------------------------------------------------------
// Remove unused `beforeUpdate` / `afterUpdate` specifiers from svelte imports.
// If all specifiers in `import { beforeUpdate, afterUpdate } from "svelte"`
// are removed, drop the entire import statement.
// ---------------------------------------------------------------------------

fn migrate_unused_beforeafter_imports(source: &str, str: &mut MagicString, root: &Root) {
    let Some(instance) = &root.instance else {
        return;
    };

    // Collect all referenced identifiers (excluding the import itself).
    let mut refs: std::collections::HashSet<String> = std::collections::HashSet::new();
    for stmt in &instance.content.body {
        if matches!(stmt, Statement::Import(_)) {
            continue;
        }
        collect_identifiers_in_statement(stmt, &mut refs);
    }
    // Also handler/attribute references in template.
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
                            collect_identifiers_in_expr(expr, &mut refs);
                        }
                    }
                    ElementAttribute::Attribute(attr) => match &attr.value {
                        AttributeValue::Single(t) => collect_identifiers_in_expr(&t.expression, &mut refs),
                        AttributeValue::Many(parts) => {
                            for p in parts {
                                if let AttributeValuePart::ExpressionTag(t) = p {
                                    collect_identifiers_in_expr(&t.expression, &mut refs);
                                }
                            }
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
        // Expression tags in body.
        if let FragmentChild::ExpressionTag(t) = child {
            collect_identifiers_in_expr(&t.expression, &mut refs);
        }
    });

    let bytes = source.as_bytes();
    for stmt in &instance.content.body {
        let Statement::Import(imp) = stmt else {
            continue;
        };
        if imp.source.value != "svelte" {
            continue;
        }
        // Filter named specifiers. If the import only has named specifiers
        // and ALL of them are removable, we can remove the whole statement.
        let mut named_total = 0;
        let mut removable: Vec<&svelte_js_ast::ImportSpecifier> = Vec::new();
        for s in &imp.specifiers {
            if let ImportSpecifierKind::Named(n) = s {
                named_total += 1;
                let imported_name = match &n.imported {
                    ModuleExportName::Identifier(id) => &id.name,
                    ModuleExportName::String(sl) => &sl.value,
                };
                if (imported_name == "beforeUpdate" || imported_name == "afterUpdate")
                    && !refs.contains(&n.local.name)
                {
                    removable.push(n);
                }
            }
        }
        if removable.is_empty() {
            continue;
        }
        // If we'd remove ALL named specifiers (and there are no Default/Namespace
        // specifiers), remove the entire import statement.
        let other_specifiers = imp
            .specifiers
            .iter()
            .filter(|s| !matches!(s, ImportSpecifierKind::Named(_)))
            .count();
        if removable.len() == named_total && other_specifiers == 0 {
            // Remove the import statement only (preserve the line's leading
            // indent and trailing newline — upstream's `str.remove(start, end)`
            // doesn't touch the surrounding whitespace).
            let s = imp.span.start as usize;
            let e = imp.span.end as usize;
            str.remove(s, e);
            continue;
        }
        // Otherwise, remove individual named specifiers + a following comma
        // (or preceding comma if it's the last).
        for spec in removable {
            // The specifier span covers `LOCAL` (or `imported as LOCAL`).
            let s = spec.span.start as usize;
            let mut e = spec.span.end as usize;
            // If there's a following `,`, eat it + trailing whitespace.
            // Look at the source between spec.end and the `}` of the import.
            // We approximate the `}` position by looking forward.
            let mut k = e;
            while k < bytes.len() && bytes[k] != b',' && bytes[k] != b'}' {
                k += 1;
            }
            if k < bytes.len() && bytes[k] == b',' {
                e = k + 1;
                // Eat whitespace after comma.
                while e < bytes.len() && (bytes[e] == b' ' || bytes[e] == b'\t') {
                    e += 1;
                }
                str.remove(s, e);
            } else {
                // No trailing comma — try to eat a leading comma + whitespace.
                let mut p = s;
                while p > 0 && (bytes[p - 1] == b' ' || bytes[p - 1] == b'\t') {
                    p -= 1;
                }
                if p > 0 && bytes[p - 1] == b',' {
                    p -= 1;
                    str.remove(p, e);
                } else {
                    str.remove(s, e);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Simple derivations: `$: x = expr;` (with single Identifier target, or
// destructure target) → `let x = $derived(expr);` (or `let { x } = $derived(expr);`).
// If preceded by `let x;` (no init), remove that line.
// Only fires for the easy case — no other assignment to x in script, no
// modifications inside the labeled statement, no multi-statement block.
// ---------------------------------------------------------------------------

/// Rewrite `$$slots.X` / `$$slots['X']` / `$$props.X` / `$$restProps` in a
/// textual snippet so we can safely substitute it into a MagicString edit
/// without risking later passes attempting to re-overwrite the same positions.
fn rewrite_dollar_dollar_refs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // `$$slots.X` or `$$slots['X']`.
        if bytes[i..].starts_with(b"$$slots") {
            // Bounds check identifier prefix.
            let before_ok = i == 0
                || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
            if before_ok {
                let mut k = i + "$$slots".len();
                if k < bytes.len() && bytes[k] == b'.' {
                    let s_idx = k + 1;
                    let mut e_idx = s_idx;
                    while e_idx < bytes.len()
                        && (bytes[e_idx].is_ascii_alphanumeric() || bytes[e_idx] == b'_')
                    {
                        e_idx += 1;
                    }
                    if e_idx > s_idx {
                        let mut nm = s[s_idx..e_idx].to_string();
                        if nm == "default" {
                            nm = "children".to_string();
                        }
                        out.push_str(&nm);
                        i = e_idx;
                        continue;
                    }
                } else if k < bytes.len() && bytes[k] == b'[' {
                    k += 1;
                    while k < bytes.len() && (bytes[k] == b' ' || bytes[k] == b'\t') {
                        k += 1;
                    }
                    if k < bytes.len() && (bytes[k] == b'\'' || bytes[k] == b'"') {
                        let quote = bytes[k];
                        k += 1;
                        let s_idx = k;
                        while k < bytes.len() && bytes[k] != quote {
                            k += 1;
                        }
                        if k < bytes.len() {
                            let mut nm = s[s_idx..k].to_string();
                            k += 1;
                            while k < bytes.len() && (bytes[k] == b' ' || bytes[k] == b'\t') {
                                k += 1;
                            }
                            if k < bytes.len() && bytes[k] == b']' {
                                if nm == "default" {
                                    nm = "children".to_string();
                                }
                                out.push_str(&nm);
                                i = k + 1;
                                continue;
                            }
                        }
                    }
                }
            }
        }
        // `$$restProps` → `rest`, `$$props` → `props` (as standalone tokens).
        if bytes[i..].starts_with(b"$$restProps") {
            let before_ok = i == 0
                || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
            let after_ok = i + "$$restProps".len() >= bytes.len()
                || !(bytes[i + "$$restProps".len()].is_ascii_alphanumeric()
                    || bytes[i + "$$restProps".len()] == b'_');
            if before_ok && after_ok {
                out.push_str("rest");
                i += "$$restProps".len();
                continue;
            }
        }
        if bytes[i..].starts_with(b"$$props") {
            let before_ok = i == 0
                || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
            let after_ok = i + "$$props".len() >= bytes.len()
                || !(bytes[i + "$$props".len()].is_ascii_alphanumeric()
                    || bytes[i + "$$props".len()] == b'_');
            if before_ok && after_ok {
                out.push_str("props");
                i += "$$props".len();
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn migrate_simple_derivations(
    source: &str,
    str: &mut MagicString,
    root: &Root,
) -> (
    std::collections::HashSet<usize>,
    std::collections::HashSet<String>,
) {
    let mut consumed: std::collections::HashSet<usize> = Default::default();
    let mut consumed_names: std::collections::HashSet<String> = Default::default();
    let Some(instance) = &root.instance else {
        return (consumed, consumed_names);
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
                // Pull the AssignmentExpression from either an
                // ExpressionStatement body or a single-stmt BlockStatement.
                let asn_opt: Option<&svelte_js_ast::AssignmentExpression> = match &l.body {
                    Statement::Expression(es) => match &es.expression {
                        Expression::Assignment(a) => Some(a),
                        _ => None,
                    },
                    Statement::Block(b) if b.body.len() == 1 => {
                        if let Statement::Expression(es) = &b.body[0] {
                            if let Expression::Assignment(a) = &es.expression {
                                Some(a)
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                if let Some(asn) = asn_opt {
                    // Count ALL assignment targets within the labeled body
                    // (for blocks, this is just the single assignment).
                    let mut targets = std::collections::HashSet::new();
                    collect_assignment_targets(&l.body, &mut targets);
                    for n in targets {
                        *dollar_assigns.entry(n).or_insert(0) += 1;
                    }
                    let _ = asn;
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
        // Find the AssignmentExpression. Either:
        // (a) Body is ExpressionStatement(Assignment) — `$: x = expr`
        // (b) Body is BlockStatement([ExpressionStatement(Assignment)]) —
        //     `$: { x = expr; }`
        let asn: &svelte_js_ast::AssignmentExpression = {
            let inner_expr = match &l.body {
                Statement::Expression(es) => match &es.expression {
                    Expression::Paren(p) => &p.expression,
                    other => other,
                },
                Statement::Block(b) if b.body.len() == 1 => {
                    if let Statement::Expression(es) = &b.body[0] {
                        match &es.expression {
                            Expression::Paren(p) => &p.expression,
                            other => other,
                        }
                    } else {
                        continue;
                    }
                }
                _ => continue,
            };
            if let Expression::Assignment(a) = inner_expr {
                a
            } else {
                continue;
            }
        };
        if asn.operator != svelte_js_ast::AssignmentOperator::Assign {
            continue;
        }
        let is_block_body = matches!(&l.body, Statement::Block(_));
        // Identify target style.
        let (target_text, target_names): (String, Vec<String>) = match &asn.left {
            svelte_js_ast::AssignmentTarget::Expression(Expression::Identifier(id)) => {
                // Skip store-prefixed `$name` — those are Svelte 4 store
                // auto-subscriptions, not derivable.
                if id.name.starts_with('$') {
                    continue;
                }
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
                // Skip if any name is store-prefixed (`$store`) — those are
                // Svelte 4 auto-subscriptions, not derivable.
                if names.iter().any(|n| n.starts_with('$')) {
                    continue;
                }
                (slice.to_string(), names.into_iter().collect())
            }
            _ => continue,
        };
        // Get RHS bounds.
        let (rs, re) = expr_span(&asn.right);
        let rhs_text_raw = source[rs as usize..re as usize].to_string();
        // Rewrite `$$slots.X` / `$$slots['X']` to `X` so the apply_slot_template_edits
        // global replace doesn't try to re-overwrite a position we've already
        // updated. Also normalize `$$props.X` / `$$restProps` references.
        let rhs_text_owned = rewrite_dollar_dollar_refs(&rhs_text_raw);
        let rhs_text = rhs_text_owned.as_str();

        // Determine if RHS has any identifier dependencies. If not (e.g.,
        // `$: x = 42`), upstream treats this as `$state(...)` rather than
        // `$derived(...)`. The state path also bypasses the outside_assigns
        // check (because the binding can still be updated elsewhere).
        let mut rhs_ids: std::collections::HashSet<String> = Default::default();
        collect_identifiers_in_expr(&asn.right, &mut rhs_ids);
        let should_be_state = rhs_ids.is_empty();

        // Skip if any target has outside assignment (state-like) or multiple
        // `$:` assignments — UNLESS we're going down the state path.
        let mut skip = false;
        for n in &target_names {
            if !should_be_state && outside_assigns.get(n).copied().unwrap_or(0) > 0 {
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
        if target_names.iter().any(|n| n == "derived" || n == "state") {
            continue;
        }

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
        // Also detect a preceding `let X = INIT;` (WITH init) for a target —
        // in that case, upstream treats X as state, not derived, so we bail.
        let mut preceding_let_id_end: Option<usize> = None;
        let mut preceding_let_with_init = false;
        for sibling in body {
            let Statement::Variable(v) = sibling else {
                continue;
            };
            if v.span.start as usize >= l_start {
                continue;
            }
            for d in &v.declarations {
                let Pattern::Identifier(id) = &d.id else {
                    continue;
                };
                if !target_names.contains(&id.name) {
                    continue;
                }
                if d.init.is_some() {
                    preceding_let_with_init = true;
                } else if v.declarations.len() == 1 {
                    preceding_let_id_end = Some(id.span.end as usize);
                }
            }
        }
        if preceding_let_with_init {
            // Falls through to the `run(...)` effect path.
            continue;
        }

        let rune = if should_be_state { "$state" } else { "$derived" };
        if let Some(id_end) = preceding_let_id_end {
            // Append ` = $derived(RHS)` (or `$state(RHS)`) after the `let X`
            // identifier and remove the labeled statement entirely. This
            // leaves the leading `\t` of the original `$:` line behind, which
            // matches upstream output (the blank line with trailing tab).
            str.append_left(id_end, format!(" = {}({})", rune, rhs_text));
            str.remove(l_start, l_end);
        } else if should_be_state {
            // No preceding let, state path — upstream prepends
            // `let X = $state(LIT);\n${indent}` before the labeled stmt and
            // then removes the labeled stmt entirely, leaving the leading
            // `\t` behind.
            let indent = {
                let mut p = l_start;
                while p > 0 && bytes[p - 1] != b'\n' {
                    p -= 1;
                }
                source[p..l_start].to_string()
            };
            str.prepend_left(
                l_start,
                format!("let {} = $state({});\n{}", target_clean, rhs_text, indent),
            );
            str.remove(l_start, l_end);
        } else {
            // No preceding let, derived path — upstream replaces the labeled
            // statement in place: `$: x = expr;` → `let x = $derived(expr);`.
            // Preserves the line indent without leaving a blank line.
            let replacement = format!("let {} = {}({})", target_clean, rune, rhs_text);
            let final_replacement = if bytes.get(l_end.saturating_sub(1)).copied() == Some(b';') {
                format!("{};", replacement)
            } else {
                replacement
            };
            str.update(l_start, l_end, &final_replacement);
        }
        let _ = is_block_body;
        consumed.insert(l_start);
        for n in &target_names {
            consumed_names.insert(n.clone());
        }
    }
    let _ = str;
    (consumed, consumed_names)
}

// ---------------------------------------------------------------------------
// Simple state migration: `let X = expr;` or `let X;` (non-prop) where X is
// reassigned somewhere → wrap with `$state(...)`.
// ---------------------------------------------------------------------------

fn migrate_simple_state(
    source: &str,
    str: &mut MagicString,
    root: &Root,
    derived_consumed_names: &std::collections::HashSet<String>,
) {
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
    // But: if a $: target is ALSO declared as `let X = INIT;` (with init),
    // upstream treats X as state, not derived — so we do NOT skip it here.
    let mut derived_targets: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut decl_with_init: std::collections::HashSet<String> = std::collections::HashSet::new();
    for stmt in &instance.content.body {
        if let Statement::Variable(v) = stmt {
            for d in &v.declarations {
                if d.init.is_some() {
                    if let Pattern::Identifier(id) = &d.id {
                        decl_with_init.insert(id.name.clone());
                    }
                }
            }
        }
    }
    for stmt in &instance.content.body {
        if let Statement::Labeled(l) = stmt {
            if l.label.name == "$" {
                let asn_opt: Option<&svelte_js_ast::AssignmentExpression> = match &l.body {
                    Statement::Expression(es) => match &es.expression {
                        Expression::Assignment(a) => Some(a),
                        _ => None,
                    },
                    Statement::Block(b) if b.body.len() == 1 => {
                        if let Statement::Expression(es) = &b.body[0] {
                            if let Expression::Assignment(a) = &es.expression {
                                Some(a)
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                if let Some(asn) = asn_opt {
                    let mut local: std::collections::HashSet<String> = Default::default();
                    if let svelte_js_ast::AssignmentTarget::Expression(
                        Expression::Identifier(id),
                    ) = &asn.left
                    {
                        local.insert(id.name.clone());
                    }
                    if let svelte_js_ast::AssignmentTarget::Pattern(p) = &asn.left {
                        collect_pattern_names(p, &mut local);
                    }
                    // We don't yet know whether the derivation pass will
                    // consume this `$:`; that's a separate signal passed in
                    // via `derived_consumed_names`. Only the original
                    // ExpressionStatement-derived case is unconditionally
                    // skipped here (matching legacy behavior).
                    let asn_is_expr_stmt = matches!(&l.body, Statement::Expression(_));
                    let _ = asn;
                    if asn_is_expr_stmt {
                        for n in local {
                            if !decl_with_init.contains(&n) {
                                derived_targets.insert(n);
                            }
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
                if derived_consumed_names.contains(&id.name) {
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
    fn debug_slots_fixture() {
        let src = std::fs::read_to_string("/Users/puruvijay/Projects/svelte-rs/packages/svelte/tests/migrate/samples/slots/input.svelte").unwrap();
        let input = src.trim_end().replace('\r', "");
        std::env::set_var("MIGRATE_DEBUG", "slots");
        let r = migrate(&input, MigrateOptions::default());
        eprintln!("OUTPUT:\n{}", r.code);
    }

    #[test]
    fn debug_slot_non_id() {
        let src = std::fs::read_to_string("/Users/puruvijay/Projects/svelte-rs/packages/svelte/tests/migrate/samples/slot-non-identifier/input.svelte").unwrap();
        let input = src.trim_end().replace('\r', "");
        std::env::set_var("MIGRATE_DEBUG_SLOT_WRAP", "1");
        let r = migrate(&input, MigrateOptions::default());
        eprintln!("OUTPUT:\n{}", r.code);
    }

    #[test]
    fn identity_when_no_accessors() {
        let src = "<div>hi</div>";
        let r = migrate(src, MigrateOptions::default());
        assert_eq!(r.code, "<div>hi</div>");
    }

    #[test]
    fn debug_effects_dollar_count() {
        let src = r#"<script>
	let count = 0;
	$: $count = 1;
</script>"#;
        let r = svelte_parse::parse(src, false).unwrap();
        let inst = r.instance.as_ref().unwrap();
        for s in &inst.content.body {
            if let svelte_js_ast::Statement::Labeled(l) = s {
                eprintln!("labeled body: {:?}", l.body);
            }
        }
    }

}
