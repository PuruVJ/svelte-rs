//! Port of `print()` from `node_modules/esrap@2.2.4/src/index.js:54-160`.

use serde_json::Value;

use crate::context::{Command, CommentState, Context, PrintOptions, VisitorTable};

/// One mapping segment: `(generated_column, source_index, original_line, original_column)`.
/// Source index is always 0 (esrap supports a single source).
pub type Segment = (u32, u32, u32, u32);

pub struct PrintResult {
    pub code: String,
    /// Decoded mappings, line-by-line. Encoding to VLQ happens lazily — call
    /// [`encode_mappings`] when a final `.map` file is needed.
    pub mappings: Vec<Vec<Segment>>,
    pub source_map_source: Option<String>,
    pub source_map_content: Option<String>,
}

/// Encode a decoded mapping array to VLQ string form (the `mappings` field of
/// a v3 sourcemap). Wraps the `sourcemap` crate's encoder.
pub fn encode_mappings(mappings: &[Vec<Segment>]) -> String {
    // Encode by walking segments per line and VLQ-encoding directly, mirroring
    // `@jridgewell/sourcemap-codec`'s `encode()`. The `sourcemap` crate is used
    // only for its v3 JSON serialization elsewhere.
    let mut buf: Vec<u8> = Vec::new();
    encode_decoded(mappings, &mut buf);
    String::from_utf8(buf).unwrap()
}

/// VLQ-encode decoded mappings into the `mappings` field format. Mirrors the
/// behavior of `@jridgewell/sourcemap-codec`'s `encode()`.
fn encode_decoded(mappings: &[Vec<Segment>], out: &mut Vec<u8>) {
    let mut last_src_id: i64 = 0;
    let mut last_src_line: i64 = 0;
    let mut last_src_col: i64 = 0;

    for (i, line) in mappings.iter().enumerate() {
        if i > 0 {
            out.push(b';');
        }
        let mut last_gen_col: i64 = 0;
        for (j, seg) in line.iter().enumerate() {
            if j > 0 {
                out.push(b',');
            }
            let gen_col = seg.0 as i64;
            vlq_encode(gen_col - last_gen_col, out);
            last_gen_col = gen_col;

            let src_id = seg.1 as i64;
            vlq_encode(src_id - last_src_id, out);
            last_src_id = src_id;

            let src_line = seg.2 as i64;
            vlq_encode(src_line - last_src_line, out);
            last_src_line = src_line;

            let src_col = seg.3 as i64;
            vlq_encode(src_col - last_src_col, out);
            last_src_col = src_col;
        }
    }
}

const BASE64_CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn vlq_encode(value: i64, out: &mut Vec<u8>) {
    let mut v: u64 = if value < 0 {
        (((-value) as u64) << 1) | 1
    } else {
        (value as u64) << 1
    };
    loop {
        let mut digit = (v & 0x1f) as u8;
        v >>= 5;
        if v != 0 {
            digit |= 0x20;
        }
        out.push(BASE64_CHARS[digit as usize]);
        if v == 0 {
            break;
        }
    }
}

/// Walk an estree-shaped JSON AST and emit it as JS source. Mirrors
/// `print()` in `esrap/src/index.js:54-160`.
pub fn print(node: &Value, visitors: &VisitorTable, opts: &PrintOptions) -> PrintResult {
    let cstate = CommentState::new(opts.comments.clone());
    let mut context = Context::with_comments(visitors, cstate);
    context.visit(node);

    let commands = std::mem::take(&mut context.commands);

    let mut code = String::new();
    let mut current_column: u32 = 0;
    let mut mappings: Vec<Vec<Segment>> = Vec::new();
    let mut current_line: Vec<Segment> = Vec::new();

    let indent_str = opts.indent.as_deref().unwrap_or("\t").to_string();
    let mut current_newline = String::from("\n");

    let mut needs_newline = false;
    let mut needs_margin = false;
    let mut needs_space = false;

    fn append(
        s: &str,
        code: &mut String,
        current_column: &mut u32,
        mappings: &mut Vec<Vec<Segment>>,
        current_line: &mut Vec<Segment>,
    ) {
        code.push_str(s);
        for ch in s.chars() {
            if ch == '\n' {
                mappings.push(std::mem::take(current_line));
                *current_column = 0;
            } else {
                *current_column += 1;
            }
        }
    }

    fn run(
        command: &Command,
        code: &mut String,
        current_column: &mut u32,
        mappings: &mut Vec<Vec<Segment>>,
        current_line: &mut Vec<Segment>,
        current_newline: &mut String,
        indent_str: &str,
        needs_newline: &mut bool,
        needs_margin: &mut bool,
        needs_space: &mut bool,
    ) {
        match command {
            Command::Group(cs) => {
                for c in cs {
                    run(
                        c,
                        code,
                        current_column,
                        mappings,
                        current_line,
                        current_newline,
                        indent_str,
                        needs_newline,
                        needs_margin,
                        needs_space,
                    );
                }
                return;
            }
            Command::Newline => {
                *needs_newline = true;
                return;
            }
            Command::Margin => {
                *needs_margin = true;
                return;
            }
            Command::Space => {
                *needs_space = true;
                return;
            }
            Command::Indent => {
                current_newline.push_str(indent_str);
                return;
            }
            Command::Dedent => {
                let new_len = current_newline.len().saturating_sub(indent_str.len());
                current_newline.truncate(new_len);
                return;
            }
            _ => {}
        }

        if *needs_newline {
            if *needs_margin {
                let nl = format!("\n{current_newline}");
                append(&nl, code, current_column, mappings, current_line);
            } else {
                let nl = current_newline.clone();
                append(&nl, code, current_column, mappings, current_line);
            }
        } else if *needs_space {
            append(" ", code, current_column, mappings, current_line);
        }

        *needs_margin = false;
        *needs_newline = false;
        *needs_space = false;

        match command {
            Command::Str(s) => append(s, code, current_column, mappings, current_line),
            Command::Location { line, column } => {
                current_line.push((*current_column, 0, line.saturating_sub(1), *column));
            }
            _ => {}
        }
    }

    for cmd in &commands {
        run(
            cmd,
            &mut code,
            &mut current_column,
            &mut mappings,
            &mut current_line,
            &mut current_newline,
            &indent_str,
            &mut needs_newline,
            &mut needs_margin,
            &mut needs_space,
        );
    }
    mappings.push(current_line);

    PrintResult {
        code,
        mappings,
        source_map_source: opts.source_map_source.clone(),
        source_map_content: opts.source_map_content.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ident_visitor(node: &Value, ctx: &mut Context) {
        ctx.write(node["name"].as_str().unwrap(), Some(node));
    }

    fn literal_visitor(node: &Value, ctx: &mut Context) {
        ctx.write(node["raw"].as_str().unwrap_or("null"), Some(node));
    }

    fn make_table() -> VisitorTable {
        let mut t: VisitorTable = std::collections::HashMap::new();
        t.insert("Identifier", ident_visitor);
        t.insert("Literal", literal_visitor);
        t
    }

    #[test]
    fn prints_bare_identifier() {
        let v = make_table();
        let node = json!({"type": "Identifier", "name": "foo"});
        let r = print(&node, &v, &PrintOptions::default());
        assert_eq!(r.code, "foo");
    }

    #[test]
    fn prints_literal() {
        let v = make_table();
        let node = json!({"type": "Literal", "raw": "42"});
        let r = print(&node, &v, &PrintOptions::default());
        assert_eq!(r.code, "42");
    }
}
