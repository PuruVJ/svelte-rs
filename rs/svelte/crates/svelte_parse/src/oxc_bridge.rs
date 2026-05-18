//! OXC parser front-end + typed conversion.
//!
//! `parse_*` entry points parse a source slice via OXC, then convert the
//! arena-allocated OXC AST into owned `svelte_js_ast` nodes via
//! [`crate::oxc_to_typed`]. All node spans are shifted into the original
//! `.svelte` source's coordinate space.

use oxc_allocator::Allocator;
use oxc_parser::{ParseOptions, Parser as OxcParser};
use oxc_span::SourceType;
use svelte_diagnostics::{errors, CompileDiagnostic};
use svelte_js_ast::{Expression, Pattern, Program};

use crate::oxc_to_typed::{self as walker, Shift};
use crate::utils::locator::LineMap;

#[derive(Debug, Clone)]
pub struct RawComment {
    pub line: bool,
    pub start: u32,
    pub end: u32,
    pub value: String,
    pub with_character: bool,
}

fn opts() -> ParseOptions {
    ParseOptions { preserve_parens: true, ..ParseOptions::default() }
}

fn js_diag(start: usize, end: usize, msg: String) -> CompileDiagnostic {
    errors::js_parse_error(Some((start as u32, end as u32)), &msg)
}

fn collect_comments(
    allocator_comments: &[oxc_ast::Comment],
    slice: &str,
    shift_offset: u32,
    with_character: bool,
) -> Vec<RawComment> {
    allocator_comments
        .iter()
        .map(|c| {
            let span = c.span;
            let value = slice[(span.start as usize)..(span.end as usize)].to_string();
            // OXC's Comment.span covers the comment's *body* (between the
            // `//` or `/*` markers and the terminator). Adjust to include
            // the markers when emitting a `RawComment` — acorn's onComment
            // shape carries the start of `//` or `/*`.
            let kind_offset = if matches!(c.kind, oxc_ast::CommentKind::Line) {
                2 // `//`
            } else {
                2 // `/*` ... `*/`
            };
            let start = span.start.saturating_sub(kind_offset) + shift_offset;
            let end = if matches!(c.kind, oxc_ast::CommentKind::Line) {
                span.end + shift_offset
            } else {
                span.end + 2 + shift_offset // include `*/`
            };
            RawComment {
                line: matches!(c.kind, oxc_ast::CommentKind::Line),
                start,
                end,
                value,
                with_character,
            }
        })
        .collect()
}

pub fn parse_expression(
    full_source: &str,
    _line_map: &LineMap,
    start: usize,
    end: usize,
    ts: bool,
) -> Result<Expression, CompileDiagnostic> {
    let slice = &full_source[start..end];
    let allocator = Allocator::default();
    let source_type = SourceType::default().with_typescript(ts);
    let parser = OxcParser::new(&allocator, slice, source_type).with_options(opts());
    match parser.parse_expression() {
        Ok(expr) => Ok(walker::expression(&expr, Shift(start as u32))),
        Err(diags) => {
            let msg = diags.iter().map(|d| format!("{d}")).collect::<Vec<_>>().join("; ");
            Err(js_diag(start, end, msg))
        }
    }
}

pub fn parse_program(
    full_source: &str,
    _line_map: &LineMap,
    start: usize,
    end: usize,
    ts: bool,
) -> Result<(Program, Vec<RawComment>), CompileDiagnostic> {
    let slice = &full_source[start..end];
    let allocator = Allocator::default();
    let source_type = SourceType::default().with_typescript(ts).with_module(true);
    let parser = OxcParser::new(&allocator, slice, source_type).with_options(opts());
    let ret = parser.parse();
    if !ret.errors.is_empty() {
        let msg = ret.errors.iter().map(|d| format!("{d}")).collect::<Vec<_>>().join("; ");
        return Err(js_diag(start, end, msg));
    }
    let prog = walker::program(&ret.program, Shift(start as u32));
    let comments = collect_comments(&ret.program.comments, slice, start as u32, true);
    Ok((prog, comments))
}

pub fn parse_expression_at(
    full_source: &str,
    line_map: &LineMap,
    start: usize,
    ts: bool,
) -> Result<(Expression, usize), CompileDiagnostic> {
    let (e, end, _) = parse_expression_at_with_comments(full_source, line_map, start, ts)?;
    Ok((e, end))
}

pub fn parse_expression_at_with_comments(
    full_source: &str,
    _line_map: &LineMap,
    start: usize,
    ts: bool,
) -> Result<(Expression, usize, Vec<RawComment>), CompileDiagnostic> {
    let tail = &full_source[start..];
    let allocator = Allocator::default();
    let source_type = SourceType::default().with_typescript(ts);
    // Parse the WHOLE tail as a program — OXC reports the end span of the
    // first expression, which we use to advance the cursor.
    let parser = OxcParser::new(&allocator, tail, source_type).with_options(opts());
    let res = parser.parse_expression();
    match res {
        Ok(expr) => {
            let span_end = oxc_span::GetSpan::span(&expr).end as usize + start;
            let expression = walker::expression(&expr, Shift(start as u32));
            Ok((expression, span_end, Vec::new()))
        }
        Err(diags) => {
            let msg = diags.iter().map(|d| format!("{d}")).collect::<Vec<_>>().join("; ");
            Err(js_diag(start, start + tail.len(), msg))
        }
    }
}

/// Parse a `NAME = EXPR` slice (the body of `{@const NAME = EXPR}`) as a
/// VariableDeclaration. Wraps the text with `const ` then runs OXC, extracts
/// the single VariableDeclaration, and shifts spans back to the original
/// source coordinates.
pub fn parse_const_decl_at(
    full_source: &str,
    _line_map: &LineMap,
    start: usize,
    end: usize,
    ts: bool,
) -> Result<svelte_js_ast::VariableDeclaration, CompileDiagnostic> {
    let body = &full_source[start..end];
    let synthetic = format!("const {body};");
    let allocator = Allocator::default();
    let source_type = SourceType::default().with_typescript(ts).with_module(true);
    let parser = OxcParser::new(&allocator, &synthetic, source_type).with_options(opts());
    let ret = parser.parse();
    if !ret.errors.is_empty() {
        let msg = ret.errors.iter().map(|d| format!("{d}")).collect::<Vec<_>>().join("; ");
        return Err(js_diag(start, end, msg));
    }
    let prefix_len = 6u32; // "const "
    let shift = Shift((start as i64 - prefix_len as i64).max(0) as u32);
    let prog = walker::program(&ret.program, shift);
    // Expect a single Variable statement.
    for stmt in prog.body {
        if let svelte_js_ast::Statement::Variable(v) = stmt {
            return Ok(*v);
        }
    }
    Err(js_diag(start, end, "expected const declaration".into()))
}

pub fn parse_pattern_at(
    full_source: &str,
    _line_map: &LineMap,
    start: usize,
    end: usize,
    ts: bool,
) -> Result<(Pattern, usize), CompileDiagnostic> {
    // Re-parse using the synthetic-source trick: wrap as `let <pattern> = 0;`
    // and pluck the declarator's id back out.
    let pattern_text = &full_source[start..end];
    let synthetic = format!("let {pattern_text} = 0;");
    let allocator = Allocator::default();
    let source_type = SourceType::default().with_typescript(ts).with_module(true);
    let parser = OxcParser::new(&allocator, &synthetic, source_type).with_options(opts());
    let ret = parser.parse();
    if !ret.errors.is_empty() {
        let msg = ret.errors.iter().map(|d| format!("{d}")).collect::<Vec<_>>().join("; ");
        return Err(js_diag(start, end, msg));
    }
    let prefix_len = 4u32; // "let "
    // Position of the pattern within the synthetic source is at `prefix_len`.
    // After parsing, the BindingPattern.span.start == prefix_len. To shift
    // back to the original .svelte coordinates: `start - prefix_len`.
    let shift_value = start as i64 - prefix_len as i64;
    let shift = if shift_value < 0 {
        Shift(0)
    } else {
        Shift(shift_value as u32)
    };
    if let Some(oxc_ast::ast::Statement::VariableDeclaration(decl)) = ret.program.body.first() {
        if let Some(d) = decl.declarations.first() {
            return Ok((walker::binding_pattern(&d.id, shift), end));
        }
    }
    Err(js_diag(start, end, "expected pattern".to_string()))
}

pub fn parse_arrow_params_at(
    full_source: &str,
    _line_map: &LineMap,
    start: usize,
    end: usize,
    ts: bool,
) -> Result<(Vec<Pattern>, usize), CompileDiagnostic> {
    // Wrap as `let _f = (PARAMS) => {};` and read the arrow's params back.
    let params_text = &full_source[start..end];
    let synthetic = format!("let _f = {params_text} => {{}};");
    let allocator = Allocator::default();
    let source_type = SourceType::default().with_typescript(ts).with_module(true);
    let parser = OxcParser::new(&allocator, &synthetic, source_type).with_options(opts());
    let ret = parser.parse();
    if !ret.errors.is_empty() {
        let msg = ret.errors.iter().map(|d| format!("{d}")).collect::<Vec<_>>().join("; ");
        return Err(js_diag(start, end, msg));
    }
    let prefix_len = "let _f = ".len() as u32;
    let shift_value = start as i64 - prefix_len as i64;
    let shift = if shift_value < 0 {
        Shift(0)
    } else {
        Shift(shift_value as u32)
    };
    // Walk to the ArrowFunctionExpression.
    if let Some(oxc_ast::ast::Statement::VariableDeclaration(decl)) = ret.program.body.first() {
        if let Some(d) = decl.declarations.first() {
            if let Some(oxc_ast::ast::Expression::ArrowFunctionExpression(arrow)) = &d.init {
                let mut out: Vec<Pattern> = arrow
                    .params
                    .items
                    .iter()
                    .map(|p| walker::binding_pattern(&p.pattern, shift))
                    .collect();
                if let Some(rest) = &arrow.params.rest {
                    out.push(Pattern::Rest(Box::new(svelte_js_ast::RestElement {
                        argument: walker::binding_pattern(&rest.rest.argument, shift),
                        span: svelte_js_ast::Span::new(
                            rest.rest.span.start + shift.0,
                            rest.rest.span.end + shift.0,
                        ),
                    })));
                }
                return Ok((out, end));
            }
        }
    }
    Ok((Vec::new(), end))
}
