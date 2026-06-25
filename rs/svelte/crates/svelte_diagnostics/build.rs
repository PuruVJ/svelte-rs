//! Generates `errors.rs` and `warnings.rs` at build time from the upstream
//! Markdown files in `packages/svelte/messages/`.
//!
//! The Markdown grammar (and the resulting overload semantics) is the same one
//! understood by `packages/svelte/scripts/process-messages/index.js`. The
//! corresponding JS template lives at
//! `packages/svelte/scripts/process-messages/templates/{compile-errors,compile-warnings}.js`
//! and produces the exact message strings that this generator emits, including
//! the trailing `\nhttps://svelte.dev/e/<code>` URL line.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Walk ancestors of the manifest dir until we find `packages/svelte/messages/`.
fn locate_messages_dir() -> PathBuf {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    for ancestor in manifest.ancestors() {
        let candidate = ancestor.join("packages/svelte/messages");
        if candidate.is_dir() {
            return candidate;
        }
    }
    panic!(
        "could not locate packages/svelte/messages/ from CARGO_MANIFEST_DIR={}",
        manifest.display()
    );
}

/// One overload of a diagnostic message.
struct Overload {
    /// Text with placeholders still embedded as `%name%`.
    text: String,
    /// Variable names in order of first appearance across this and prior overloads.
    vars: Vec<String>,
}

/// All overloads for a single diagnostic code.
struct Diagnostic {
    code: String,
    overloads: Vec<Overload>,
}

/// Parse one markdown file into a list of diagnostics.
///
/// Format (per `scripts/process-messages/index.js`):
/// ```text
/// ## code_name
///
/// > message line 1
/// > message line 2 continuation
///
/// > overload with different vars
///
/// Optional details paragraph (ignored).
///
/// ## next_code
/// ...
/// ```
fn parse_md(content: &str) -> Vec<Diagnostic> {
    let content = content.replace("\r\n", "\n");
    let mut diagnostics = Vec::new();

    // Split into blocks by `## ` headers.
    let mut iter = content.split("\n## ");
    // First chunk before any header is preamble (or empty).
    let first = iter.next().unwrap();
    let first_trim = first.trim_start();
    let blocks: Vec<&str> = if let Some(rest) = first_trim.strip_prefix("## ") {
        std::iter::once(rest).chain(iter).collect()
    } else {
        iter.collect()
    };

    for block in blocks {
        let block = block.trim_end_matches('\n');
        if block.is_empty() {
            continue;
        }
        let (code_line, body) = match block.split_once('\n') {
            Some(v) => v,
            None => (block, ""),
        };
        let code = code_line.trim().to_string();
        if code.is_empty() {
            continue;
        }

        // Sections separated by blank lines.
        let sections: Vec<&str> = body
            .split("\n\n")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();

        // Sections starting with `> ` are messages; trailing non-`> ` sections
        // are details and ignored. (Matches the JS: pop trailing non-`> ` sections.)
        let mut msg_sections: Vec<&str> = Vec::new();
        for s in &sections {
            if s.starts_with("> ") || s.starts_with(">\n") || *s == ">" {
                msg_sections.push(s);
            } else if msg_sections.is_empty() {
                // pre-message details — keep scanning
                continue;
            } else {
                // trailing details — stop accumulating messages
                break;
            }
        }

        let mut accumulated_vars: Vec<String> = Vec::new();
        let mut overloads: Vec<Overload> = Vec::new();
        for section in msg_sections {
            // Strip leading `> ` (or `>` on a blank line) from each line.
            let mut text = String::new();
            for (i, line) in section.lines().enumerate() {
                let stripped = line
                    .strip_prefix("> ")
                    .or_else(|| line.strip_prefix(">"))
                    .unwrap_or(line);
                if i > 0 {
                    text.push('\n');
                }
                text.push_str(stripped);
            }

            // Pull out %placeholder% names in order, deduplicating but preserving order.
            for var in extract_placeholders(&text) {
                if !accumulated_vars.contains(&var) {
                    accumulated_vars.push(var);
                }
            }
            overloads.push(Overload {
                text,
                vars: accumulated_vars.clone(),
            });
        }

        if overloads.is_empty() {
            continue;
        }
        diagnostics.push(Diagnostic { code, overloads });
    }

    diagnostics
}

fn extract_placeholders(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len()
                && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_')
            {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'%' && j > start {
                let name = std::str::from_utf8(&bytes[start..j]).unwrap().to_string();
                out.push(name);
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Emit one Rust function for the given diagnostic.
fn emit_function(out: &mut String, diag: &Diagnostic, suffix_url: &str) {
    // Required params: the vars from the FIRST overload.
    // Optional params: any additional vars introduced by later overloads.
    let first_vars = &diag.overloads.first().unwrap().vars;
    let all_vars = &diag.overloads.last().unwrap().vars;
    let required: Vec<&String> = first_vars.iter().collect();
    let optional: Vec<&String> = all_vars
        .iter()
        .filter(|v| !first_vars.contains(v))
        .collect();

    // Function signature.
    out.push_str("/// `");
    out.push_str(&diag.code);
    out.push_str("`\n///\n");
    for o in &diag.overloads {
        out.push_str("/// ");
        for (i, line) in o.text.lines().enumerate() {
            if i > 0 {
                out.push_str("\n/// ");
            }
            out.push_str(line);
        }
        out.push('\n');
    }
    out.push_str("pub fn ");
    out.push_str(&sanitize_ident(&diag.code));
    out.push_str("(span: Option<Span>");
    for v in &required {
        out.push_str(", ");
        out.push_str(&sanitize_ident(v));
        out.push_str(": &str");
    }
    for v in &optional {
        out.push_str(", ");
        out.push_str(&sanitize_ident(v));
        out.push_str(": Option<&str>");
    }
    out.push_str(") -> CompileDiagnostic {\n");

    // Build message body. We walk overloads from last (most-vars) to first (least)
    // and emit an if/else ladder gated on the presence of optional vars.
    out.push_str("    let mut __m = String::new();\n");
    if diag.overloads.len() == 1 {
        emit_concat(out, &diag.overloads[0], "    ");
    } else {
        // Emit if/else chain: most-specific overload first.
        let last_idx = diag.overloads.len() - 1;
        for (i, overload) in diag.overloads.iter().enumerate().rev() {
            // For overload N, all vars added since overload N-1 must be Some(_).
            let prev_count = if i == 0 {
                0
            } else {
                diag.overloads[i - 1].vars.len()
            };
            let new_vars: Vec<&String> = overload.vars[prev_count..].iter().collect();

            if i == last_idx {
                if i == 0 {
                    // Only overload — no condition.
                    emit_concat(out, overload, "    ");
                } else {
                    out.push_str("    if ");
                    for (j, v) in new_vars.iter().enumerate() {
                        if j > 0 {
                            out.push_str(" && ");
                        }
                        out.push_str(&sanitize_ident(v));
                        out.push_str(".is_some()");
                    }
                    out.push_str(" {\n");
                    emit_concat(out, overload, "        ");
                    out.push_str("    }");
                }
            } else if i == 0 {
                out.push_str(" else {\n");
                emit_concat(out, overload, "        ");
                out.push_str("    }\n");
            } else {
                out.push_str(" else if ");
                for (j, v) in new_vars.iter().enumerate() {
                    if j > 0 {
                        out.push_str(" && ");
                    }
                    out.push_str(&sanitize_ident(v));
                    out.push_str(".is_some()");
                }
                out.push_str(" {\n");
                emit_concat(out, overload, "        ");
                out.push_str("    }");
            }
        }
    }

    // Append URL line.
    out.push_str("    __m.push_str(\"\\n");
    out.push_str(suffix_url);
    out.push_str("\");\n    __m.push_str(\"");
    out.push_str(&diag.code);
    out.push_str("\");\n");

    out.push_str("    CompileDiagnostic { code: \"");
    out.push_str(&diag.code);
    out.push_str("\", message: __m, position: span }\n}\n\n");
}

/// Emit a sequence of `__m.push_str(...)` calls that reconstruct the overload's
/// text with placeholders substituted by their bound parameter.
fn emit_concat(out: &mut String, overload: &Overload, indent: &str) {
    // Split text on `%name%` boundaries.
    let mut buf = String::new();
    let mut i = 0;
    let bytes = overload.text.as_bytes();
    let mut chunks: Vec<(bool, String)> = Vec::new(); // (is_var, content)

    while i < bytes.len() {
        if bytes[i] == b'%' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len()
                && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_')
            {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'%' && j > start {
                if !buf.is_empty() {
                    chunks.push((false, std::mem::take(&mut buf)));
                }
                let name = std::str::from_utf8(&bytes[start..j]).unwrap().to_string();
                chunks.push((true, name));
                i = j + 1;
                continue;
            }
        }
        buf.push(bytes[i] as char);
        i += 1;
    }
    if !buf.is_empty() {
        chunks.push((false, buf));
    }

    let is_optional = |name: &str| -> bool {
        // Vars in the first overload are required; vars added later are optional.
        // We can determine optionality based on whether `name` is in vars
        // beyond what the first overload defined.
        // Note: this function is called per-overload but the optionality is a
        // diagnostic-level property. We approximate: if name appears in this
        // overload's vars but not in overload[0]'s vars (when this is overload>0),
        // it's optional.
        let _ = name;
        false
    };
    let _ = is_optional;

    for (is_var, content) in chunks {
        out.push_str(indent);
        if is_var {
            // Determine if this var is in the *first* overload (required) or only later (optional).
            // We do this by checking the overload — but actually we want to know
            // whether the formal parameter is `&str` or `Option<&str>`.
            // Since this is per-diagnostic context, we'd need that context here.
            // We handle this with a marker placeholder and patch later.
            out.push_str("__push_str_var!(");
            out.push_str(&sanitize_ident(&content));
            out.push_str(");\n");
        } else {
            out.push_str("__m.push_str(");
            out.push_str(&rust_string_literal(&content));
            out.push_str(");\n");
        }
    }
}

fn rust_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Map `code` → safe Rust identifier (keywords get an `r#` prefix).
fn sanitize_ident(s: &str) -> String {
    match s {
        "type" | "match" | "if" | "else" | "fn" | "let" | "mut" | "ref" | "self"
        | "Self" | "super" | "crate" | "extern" | "use" | "mod" | "pub" | "struct"
        | "enum" | "trait" | "impl" | "for" | "while" | "loop" | "return" | "break"
        | "continue" | "where" | "as" | "in" | "move" | "box" | "true" | "false"
        | "const" | "static" | "unsafe" | "async" | "await" | "dyn" | "yield" => {
            format!("r#{}", s)
        }
        _ => s.to_string(),
    }
}

/// Post-process the generated function bodies, replacing the
/// `__push_str_var!(ident)` markers with the right call depending on whether
/// the variable is optional in this diagnostic's signature.
fn patch_var_pushes(src: &str, diag: &Diagnostic) -> String {
    let first_vars: Vec<&String> = diag.overloads[0].vars.iter().collect();
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(idx) = rest.find("__push_str_var!(") {
        out.push_str(&rest[..idx]);
        let after = &rest[idx + "__push_str_var!(".len()..];
        let close = after.find(");\n").expect("malformed __push_str_var marker");
        let name = &after[..close];
        let raw_name = name.strip_prefix("r#").unwrap_or(name);
        let is_required = first_vars.iter().any(|v| v.as_str() == raw_name);
        if is_required {
            out.push_str("__m.push_str(");
            out.push_str(name);
            out.push_str(");\n");
        } else {
            out.push_str("if let Some(__v) = ");
            out.push_str(name);
            out.push_str(" { __m.push_str(__v); }\n");
        }
        rest = &after[close + ");\n".len()..];
    }
    out.push_str(rest);
    out
}

fn emit_module(diags: &[Diagnostic], url_prefix: &str) -> String {
    let mut out = String::new();
    out.push_str("// @generated by build.rs — do not edit.\n");
    out.push_str("use crate::{CompileDiagnostic, Span};\n\n");
    for diag in diags {
        let mut buf = String::new();
        emit_function(&mut buf, diag, url_prefix);
        let patched = patch_var_pushes(&buf, diag);
        out.push_str(&patched);
    }
    out
}

fn read_dir_sorted(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir({}) failed: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("md"))
        .collect();
    entries.sort();
    entries
}

fn collect(category_dirs: &[&Path]) -> Vec<Diagnostic> {
    let mut all = Vec::new();
    for dir in category_dirs {
        for path in read_dir_sorted(dir) {
            let content = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()));
            all.extend(parse_md(&content));
            println!("cargo:rerun-if-changed={}", path.display());
        }
        println!("cargo:rerun-if-changed={}", dir.display());
    }
    all.sort_by(|a, b| a.code.cmp(&b.code));
    all
}

fn main() {
    let messages = locate_messages_dir();
    println!("cargo:rerun-if-changed={}", messages.display());

    let errors = collect(&[
        &messages.join("compile-errors"),
        &messages.join("shared-errors"),
    ]);
    let warnings = collect(&[
        &messages.join("compile-warnings"),
        &messages.join("shared-warnings"),
    ]);

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    fs::write(
        out_dir.join("errors.rs"),
        emit_module(&errors, "https://svelte.dev/e/"),
    )
    .unwrap();
    fs::write(
        out_dir.join("warnings.rs"),
        emit_module(&warnings, "https://svelte.dev/e/"),
    )
    .unwrap();
}
