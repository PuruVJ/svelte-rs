//! Typed-AST printer.
//!
//! Consumes `svelte_js_ast` typed nodes and emits JS source as a single
//! preallocated `String`. Direct match-based dispatch — no visitor table,
//! no `serde_json::Value`, no hash lookups in the hot loop. Sourcemap
//! mappings recorded per node via its `Span`.
//!
//! Layout decisions (line wrapping, comment placement, parens) mirror
//! esrap, but the rendering is direct instead of going through the
//! Command-stream indirection the original port used. That indirection
//! existed only to mirror esrap's JS implementation strategy — Rust can
//! emit directly without the staging buffer.
//!
//! Status: minimal — supports what `hello-world` needs end-to-end. Other
//! visitors filled in as transforms migrate to the typed builders.
//!
//! Sourcemap segment shape matches `print.rs::Segment`:
//! `(generated_column, source_index_0, original_line_0_indexed, original_column)`.

use svelte_js_ast::*;

/// One mapping segment: `(generated_column, source_index, original_line, original_column)`.
/// Source index is always 0 (single source per compile).
pub type Segment = (u32, u32, u32, u32);

/// Result of `print_typed`. Same shape as the old `PrintResult` for drop-in
/// substitution at higher layers.
pub struct TypedPrintResult {
    pub code: String,
    pub mappings: Vec<Vec<Segment>>,
    pub source_map_source: Option<String>,
    pub source_map_content: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypedCommentKind {
    Line,
    Block,
}

/// A comment from the original source, threaded through to the printer so
/// it can preserve inter-statement/inter-declarator comment placement.
/// `start`/`end` are byte offsets into the original `.svelte` source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedComment {
    pub kind: TypedCommentKind,
    pub value: String,
    pub start: u32,
    pub end: u32,
}

/// Options for `print_typed`. Subset of the legacy `PrintOptions` —
/// `indent` (default tab), `source_map_source`, `source_map_content`.
#[derive(Default, Clone)]
pub struct TypedPrintOptions {
    pub indent: Option<String>,
    pub source_map_source: Option<String>,
    pub source_map_content: Option<String>,
    /// Line-map for the original source: `LineMap[i] = byte offset of
    /// line i (0-indexed)`. Used to translate node `Span` byte offsets to
    /// (line, column) for sourcemaps. If None, no sourcemap is emitted.
    pub line_map: Option<LineMap>,
    /// Source comments to preserve in the output. Ordered by `start`.
    /// Used for inter-declarator and inter-statement comment placement.
    pub comments: Vec<TypedComment>,
}

/// Map from byte offset to (line, column). Precomputed once per input.
#[derive(Default, Clone, Debug)]
pub struct LineMap {
    line_starts: Vec<u32>,
}

impl LineMap {
    pub fn new(source: &str) -> Self {
        let mut starts = vec![0u32];
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' {
                starts.push((i + 1) as u32);
            }
        }
        Self { line_starts: starts }
    }

    /// 0-indexed (line, column) for a byte offset.
    pub fn position(&self, offset: u32) -> (u32, u32) {
        // Binary search for the largest line_start <= offset.
        let idx = match self.line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        (idx as u32, offset - self.line_starts[idx])
    }
}

/// Top-level entry point.
pub fn print_typed(program: &Program, opts: &TypedPrintOptions) -> TypedPrintResult {
    let comment_index = std::cell::Cell::new(0usize);
    let mut emitter = Emitter::new(opts, &opts.comments, &comment_index);
    emitter.emit_program(program);
    emitter.finish(opts)
}

// ----- Emitter ------------------------------------------------------------

struct Emitter<'a> {
    code: String,
    /// Current column on the current line (number of chars since last `\n`).
    col: u32,
    /// Per-line mappings, finalized when emitter sees `\n`.
    mappings: Vec<Vec<Segment>>,
    current_line: Vec<Segment>,
    /// Current indentation string (one tab per level by default).
    indent: String,
    indent_unit: String,
    line_map: Option<&'a LineMap>,
    /// True after a `newline()` call: the indent string should be emitted
    /// before the next visible content. Margin lines (multiple newlines in
    /// a row) thus end up bare with no trailing indent.
    pending_indent: bool,
    /// Source comments to preserve. Shared with sub-emitters (multiline detection)
    /// via the same slice. Each call to `flush_*` advances `comment_index`.
    comments: &'a [TypedComment],
    /// Cursor into `comments`. Wrapped in `Cell` so sub-emitters can advance
    /// it through a borrow without needing &mut to the parent.
    comment_index: &'a std::cell::Cell<usize>,
}

impl<'a> Emitter<'a> {
    fn new(
        opts: &'a TypedPrintOptions,
        comments: &'a [TypedComment],
        comment_index: &'a std::cell::Cell<usize>,
    ) -> Self {
        let indent_unit = opts.indent.clone().unwrap_or_else(|| "\t".to_string());
        Self {
            code: String::with_capacity(1024),
            col: 0,
            mappings: Vec::new(),
            current_line: Vec::new(),
            indent: String::new(),
            indent_unit,
            line_map: opts.line_map.as_ref(),
            pending_indent: false,
            comments,
            comment_index,
        }
    }

    fn finish(mut self, opts: &TypedPrintOptions) -> TypedPrintResult {
        self.mappings.push(std::mem::take(&mut self.current_line));
        TypedPrintResult {
            code: self.code,
            mappings: self.mappings,
            source_map_source: opts.source_map_source.clone(),
            source_map_content: opts.source_map_content.clone(),
        }
    }

    // -- raw output --------------------------------------------------------

    fn flush_indent(&mut self) {
        if self.pending_indent {
            self.code.push_str(&self.indent);
            self.col = self.indent.chars().count() as u32;
            self.pending_indent = false;
        }
    }

    fn write(&mut self, s: &str) {
        self.flush_indent();
        self.code.push_str(s);
        for ch in s.bytes() {
            if ch == b'\n' {
                self.mappings.push(std::mem::take(&mut self.current_line));
                self.col = 0;
            } else {
                self.col += 1;
            }
        }
    }

    fn newline(&mut self) {
        // If a previous newline left pending_indent we drop it — the line
        // about to terminate had no content, so it stays empty (margin).
        self.pending_indent = false;
        self.code.push('\n');
        self.mappings.push(std::mem::take(&mut self.current_line));
        self.col = 0;
        // Indent emitted lazily on next write so multiple back-to-back
        // newlines produce bare blank lines (no trailing whitespace).
        self.pending_indent = true;
    }

    fn space(&mut self) {
        self.flush_indent();
        self.code.push(' ');
        self.col += 1;
    }

    fn indent_in(&mut self) {
        self.indent.push_str(&self.indent_unit);
    }

    fn indent_out(&mut self) {
        let len = self.indent.len().saturating_sub(self.indent_unit.len());
        self.indent.truncate(len);
    }

    /// Record a sourcemap mapping for a span at the current output column.
    fn map(&mut self, span: Span) {
        if span.is_zero() {
            return;
        }
        let Some(lm) = self.line_map else { return };
        let (line, col) = lm.position(span.start);
        self.current_line.push((self.col, 0, line, col));
    }

    // -- top-level ---------------------------------------------------------

    fn emit_program(&mut self, p: &Program) {
        self.emit_body_seq(&p.body, false);
    }

    /// Emit a sequence of statements with esrap's margin rule:
    /// `margin if current.multiline OR prev_multiline OR types_differ`.
    /// Each statement is rendered into a sub-emitter so we can inspect its
    /// multi-line status before deciding on margin between it and the
    /// preceding statement.
    fn emit_body_seq(&mut self, body: &[Statement], inside_block: bool) {
        let mut prev_type_tag: Option<u32> = None;
        let mut prev_multiline = false;
        for (i, stmt) in body.iter().enumerate() {
            if matches!(stmt, Statement::Empty(_)) {
                continue;
            }
            // Render into a child Emitter so we know its multi-line state
            // before emitting the margin.
            let mut child = Emitter {
                code: String::new(),
                col: self.col,
                mappings: Vec::new(),
                current_line: Vec::new(),
                indent: self.indent.clone(),
                indent_unit: self.indent_unit.clone(),
                line_map: self.line_map,
                pending_indent: false,
                comments: self.comments,
                comment_index: self.comment_index,
            };
            child.emit_statement(stmt);
            let child_multiline = !child.mappings.is_empty();
            let tag = stmt_type_tag(stmt);
            if i > 0 || inside_block {
                self.newline();
                let needs_margin = prev_type_tag.is_some()
                    && (child_multiline || prev_multiline || prev_type_tag != Some(tag));
                if needs_margin {
                    self.newline();
                }
            }
            // Splice the child's output into self.
            // current_line: child's pending-but-unterminated mappings.
            // mappings: closed lines from child.
            self.flush_indent();
            // mappings: child's closed lines belong to current_line context;
            // need to push current_line first if child started fresh, then
            // append child's closed mappings.
            for line in child.mappings.into_iter() {
                self.mappings.push(std::mem::take(&mut self.current_line));
                self.current_line = line;
            }
            // Adjust mappings: actually the child's first closed line is the
            // FIRST line break inside child; that means self's current_line
            // should be terminated, then child's lines start. But the loop
            // above already pushes self.current_line then assigns child's
            // line to it — that's correct.
            // But we still need to handle the child's UNCLOSED final
            // `current_line`: it should append to self's now-active line.
            self.current_line.extend(child.current_line);
            // Push the actual text. col tracked from child.
            self.code.push_str(&child.code);
            self.col = child.col;

            prev_type_tag = Some(tag);
            prev_multiline = child_multiline;
        }
    }

    // -- statements --------------------------------------------------------

    fn emit_statement(&mut self, s: &Statement) {
        match s {
            Statement::Import(d) => self.emit_import(d),
            Statement::Variable(d) => {
                self.emit_var_decl(d);
                self.write(";");
            }
            Statement::Function(f) => self.emit_function_decl(f),
            Statement::Expression(e) => {
                self.emit_expression(&e.expression);
                self.write(";");
            }
            Statement::Block(b) => self.emit_block(b),
            Statement::ExportDefault(d) => self.emit_export_default(d),
            Statement::ExportNamed(d) => self.emit_export_named(d),
            Statement::ExportAll(d) => self.emit_export_all(d),
            Statement::Return(r) => {
                self.write("return");
                if let Some(a) = &r.argument {
                    self.space();
                    self.emit_expression(a);
                }
                self.write(";");
            }
            Statement::If(i) => self.emit_if(i),
            Statement::For(f) => self.emit_for(f),
            Statement::ForIn(f) => self.emit_for_in(f),
            Statement::ForOf(f) => self.emit_for_of(f),
            Statement::While(w) => {
                self.write("while (");
                self.emit_expression(&w.test);
                self.write(") ");
                self.emit_statement(&w.body);
            }
            Statement::DoWhile(w) => {
                self.write("do ");
                self.emit_statement(&w.body);
                self.write(" while (");
                self.emit_expression(&w.test);
                self.write(");");
            }
            Statement::Break(b) => {
                self.write("break");
                if let Some(l) = &b.label {
                    self.space();
                    self.write(&l.name);
                }
                self.write(";");
            }
            Statement::Continue(c) => {
                self.write("continue");
                if let Some(l) = &c.label {
                    self.space();
                    self.write(&l.name);
                }
                self.write(";");
            }
            Statement::Empty(_) => self.write(";"),
            Statement::Debugger(_) => self.write("debugger;"),
            Statement::Class(c) => self.emit_class_decl(c),
            Statement::Throw(t) => {
                self.write("throw ");
                self.emit_expression(&t.argument);
                self.write(";");
            }
            Statement::Try(t) => self.emit_try(t),
            Statement::Labeled(l) => {
                self.write(&l.label.name);
                self.write(": ");
                self.emit_statement(&l.body);
            }
            Statement::Switch(s) => self.emit_switch(s),
            Statement::With(w) => {
                self.write("with (");
                self.emit_expression(&w.object);
                self.write(") ");
                self.emit_statement(&w.body);
            }
            Statement::Raw(text) => self.write(text),
        }
    }

    fn emit_block(&mut self, b: &BlockStatement) {
        if b.body.is_empty() {
            self.write("{}");
            return;
        }
        self.write("{");
        self.indent_in();
        self.emit_body_seq(&b.body, true);
        self.indent_out();
        self.newline();
        self.write("}");
    }

    // -- comments ----------------------------------------------------------

    /// Write a single comment to the output. Multi-line `/* ... */` block
    /// comments are split on `\n` so each line ends up on its own output
    /// line. Mirrors `comments::write_comment`.
    fn write_comment(&mut self, c: &TypedComment) {
        match c.kind {
            TypedCommentKind::Line => {
                self.write("//");
                self.write(&c.value);
            }
            TypedCommentKind::Block => {
                self.write("/*");
                let lines: Vec<&str> = c.value.split('\n').collect();
                for (i, line) in lines.iter().enumerate() {
                    if i > 0 {
                        self.newline();
                    }
                    self.write(line);
                }
                self.write("*/");
                if lines.len() > 1 {
                    self.newline();
                }
            }
        }
    }

    /// Drain comments whose start is before `to_byte`. Mirrors
    /// `comments::flush_comments_until` but indexes by byte offset.
    /// If `pad` is true, single-line block comments are followed by a space.
    fn flush_comments_until(&mut self, to_byte: u32, pad: bool) {
        loop {
            let idx = self.comment_index.get();
            let Some(c) = self.comments.get(idx) else {
                break;
            };
            if c.start >= to_byte {
                break;
            }
            // Each emitted comment lives on its own line — we write it then
            // newline (Line: always; Block: if it spanned multiple lines).
            // Sub-emit machinery places the leading newline for us via
            // `emit_body_seq`. Inside `emit_var_decl` we own the line
            // breaks ourselves.
            self.write_comment(c);
            let is_line = matches!(c.kind, TypedCommentKind::Line);
            let multiline_block = matches!(c.kind, TypedCommentKind::Block)
                && c.value.contains('\n');
            if is_line || multiline_block {
                self.newline();
            } else if pad {
                self.write(" ");
            }
            self.comment_index.set(idx + 1);
        }
    }

    // -- variables ---------------------------------------------------------

    fn emit_var_decl(&mut self, v: &VariableDeclaration) {
        self.write(v.kind.as_str());
        self.write(" ");

        // Detect any comment whose source position falls between two
        // declarators. When present, switch to a multi-line layout — each
        // declarator on its own line, comments emitted on indented lines
        // between them. Mirrors `visitors/declarations.rs::variable_declaration`.
        //
        // Many declarators are synthesized by transforms and have `span = 0`;
        // fall back to the id pattern's span (which usually retains the
        // original source position when the identifier was reused).
        fn decl_start(d: &VariableDeclarator) -> u32 {
            if d.span.start != 0 {
                return d.span.start;
            }
            pattern_start(&d.id)
        }
        fn decl_end(d: &VariableDeclarator) -> u32 {
            if d.span.end != 0 {
                return d.span.end;
            }
            pattern_end(&d.id)
        }
        fn pattern_start(p: &Pattern) -> u32 {
            match p {
                Pattern::Identifier(i) => i.span.start,
                Pattern::Array(a) => a.span.start,
                Pattern::Object(o) => o.span.start,
                Pattern::Rest(r) => r.span.start,
                Pattern::Assignment(a) => a.span.start,
                Pattern::Member(m) => m.span.start,
            }
        }
        fn pattern_end(p: &Pattern) -> u32 {
            match p {
                Pattern::Identifier(i) => i.span.end,
                Pattern::Array(a) => a.span.end,
                Pattern::Object(o) => o.span.end,
                Pattern::Rest(r) => r.span.end,
                Pattern::Assignment(a) => a.span.end,
                Pattern::Member(m) => m.span.end,
            }
        }
        let has_inter_comment = v.declarations.len() > 1 && {
            let mut found = false;
            for i in 1..v.declarations.len() {
                let after = decl_end(&v.declarations[i - 1]);
                let before = decl_start(&v.declarations[i]);
                if after == 0 || before == 0 {
                    continue;
                }
                if self
                    .comments
                    .iter()
                    .skip(self.comment_index.get())
                    .any(|c| c.start >= after && c.start < before)
                {
                    found = true;
                    break;
                }
            }
            found
        };

        if has_inter_comment {
            self.indent_in();
            for (i, d) in v.declarations.iter().enumerate() {
                if i > 0 {
                    self.write(",");
                    self.newline();
                }
                let start = decl_start(d);
                if start != 0 {
                    self.flush_comments_until(start, false);
                }
                self.emit_pattern(&d.id);
                if let Some(init) = &d.init {
                    self.write(" = ");
                    self.emit_expression(init);
                }
            }
            self.indent_out();
        } else {
            for (i, d) in v.declarations.iter().enumerate() {
                if i > 0 {
                    self.write(", ");
                }
                self.emit_pattern(&d.id);
                if let Some(init) = &d.init {
                    self.write(" = ");
                    self.emit_expression(init);
                }
            }
        }
    }

    // -- functions / classes ----------------------------------------------

    fn emit_function_decl(&mut self, f: &FunctionDeclaration) {
        if f.r#async {
            self.write("async ");
        }
        self.write("function");
        if f.generator {
            self.write("*");
        }
        if let Some(id) = &f.id {
            self.write(" ");
            self.map(id.span);
            self.write(&id.name);
        }
        self.emit_params(&f.params);
        self.write(" ");
        self.emit_block(&f.body);
    }

    fn emit_function_expr(&mut self, f: &FunctionExpression) {
        if f.r#async {
            self.write("async ");
        }
        self.write("function");
        if f.generator {
            self.write("*");
        }
        if let Some(id) = &f.id {
            self.write(" ");
            self.write(&id.name);
        }
        self.emit_params(&f.params);
        self.write(" ");
        self.emit_block(&f.body);
    }

    fn emit_params(&mut self, params: &[Pattern]) {
        self.write("(");
        for (i, p) in params.iter().enumerate() {
            if i > 0 {
                self.write(", ");
            }
            self.emit_pattern(p);
        }
        self.write(")");
    }

    fn emit_arrow(&mut self, a: &ArrowFunctionExpression) {
        if a.r#async {
            self.write("async ");
        }
        // Always emit parens around params, even for a single bare
        // identifier — that's what esrap upstream does, and our existing
        // snapshot fixtures encode that convention.
        self.emit_params(&a.params);
        self.write(" => ");
        match &a.body {
            ArrowBody::Block(b) => self.emit_block(b),
            ArrowBody::Expression(e) => {
                if matches!(e, Expression::Object(_)) {
                    self.write("(");
                    self.emit_expression(e);
                    self.write(")");
                } else {
                    self.emit_expression(e);
                }
            }
        }
    }

    fn emit_class_decl(&mut self, c: &ClassDeclaration) {
        self.write("class");
        if let Some(id) = &c.id {
            self.write(" ");
            self.write(&id.name);
        }
        if let Some(s) = &c.super_class {
            self.write(" extends ");
            self.emit_expression(s);
        }
        self.write(" ");
        self.emit_class_body(&c.body);
    }

    fn emit_class_expr(&mut self, c: &ClassExpression) {
        self.write("class");
        if let Some(id) = &c.id {
            self.write(" ");
            self.write(&id.name);
        }
        if let Some(s) = &c.super_class {
            self.write(" extends ");
            self.emit_expression(s);
        }
        self.write(" ");
        self.emit_class_body(&c.body);
    }

    fn emit_class_body(&mut self, b: &ClassBody) {
        if b.body.is_empty() {
            self.write("{}");
            return;
        }
        self.write("{");
        self.indent_in();
        // Same margin rule as statement bodies — render each member into a
        // child Emitter, decide on margin based on its multiline state vs.
        // the previous one.
        let mut prev_tag: Option<u32> = None;
        let mut prev_multiline = false;
        for member in &b.body {
            let mut child = Emitter {
                code: String::new(),
                col: self.col,
                mappings: Vec::new(),
                current_line: Vec::new(),
                indent: self.indent.clone(),
                indent_unit: self.indent_unit.clone(),
                line_map: self.line_map,
                pending_indent: false,
                comments: self.comments,
                comment_index: self.comment_index,
            };
            child.emit_class_member(member);
            let child_multiline = !child.mappings.is_empty();
            let tag = class_member_tag(member);
            let needs_margin = prev_tag.is_some()
                && (child_multiline || prev_multiline || prev_tag != Some(tag));
            self.newline();
            if needs_margin {
                self.newline();
            }
            self.flush_indent();
            for line in child.mappings.into_iter() {
                self.mappings.push(std::mem::take(&mut self.current_line));
                self.current_line = line;
            }
            self.current_line.extend(child.current_line);
            self.code.push_str(&child.code);
            self.col = child.col;
            prev_tag = Some(tag);
            prev_multiline = child_multiline;
        }
        self.indent_out();
        self.newline();
        self.write("}");
    }

    fn emit_class_member(&mut self, m: &ClassMember) {
        match m {
            ClassMember::Method(md) => {
                if md.r#static {
                    self.write("static ");
                }
                match md.kind {
                    MethodKind::Get => self.write("get "),
                    MethodKind::Set => self.write("set "),
                    _ => {}
                }
                if md.value.r#async {
                    self.write("async ");
                }
                if md.value.generator {
                    self.write("*");
                }
                self.emit_property_key(&md.key, md.computed);
                self.emit_params(&md.value.params);
                self.write(" ");
                self.emit_block(&md.value.body);
            }
            ClassMember::Property(pd) => {
                if pd.r#static {
                    self.write("static ");
                }
                self.emit_property_key(&pd.key, pd.computed);
                // Elide `= undefined`. Class fields without an initializer
                // are already `undefined` at runtime, and esrap-shaped
                // output drops the literal.
                if let Some(v) = &pd.value {
                    let is_undefined =
                        matches!(v, Expression::Identifier(id) if id.name == "undefined");
                    if !is_undefined {
                        self.write(" = ");
                        self.emit_expression(v);
                    }
                }
                self.write(";");
            }
            ClassMember::StaticBlock(sb) => {
                self.write("static ");
                if sb.body.is_empty() {
                    self.write("{}");
                } else {
                    self.write("{");
                    self.indent_in();
                    for s in &sb.body {
                        self.newline();
                        self.emit_statement(s);
                    }
                    self.indent_out();
                    self.newline();
                    self.write("}");
                }
            }
        }
    }

    // -- imports / exports -------------------------------------------------

    fn emit_import(&mut self, d: &ImportDeclaration) {
        self.write("import ");
        if d.specifiers.is_empty() {
            self.emit_string_literal(&d.source);
            self.write(";");
            return;
        }
        let mut has_default = false;
        let mut has_namespace = false;
        let mut named: Vec<&ImportSpecifier> = Vec::new();
        for s in &d.specifiers {
            match s {
                ImportSpecifierKind::Default(d) => {
                    has_default = true;
                    self.write(&d.local.name);
                }
                ImportSpecifierKind::Namespace(n) => {
                    has_namespace = true;
                    self.write("* as ");
                    self.write(&n.local.name);
                }
                ImportSpecifierKind::Named(n) => named.push(n),
            }
        }
        if (has_default || has_namespace) && !named.is_empty() {
            self.write(", ");
        }
        if !named.is_empty() {
            self.write("{ ");
            for (i, n) in named.iter().enumerate() {
                if i > 0 {
                    self.write(", ");
                }
                let imported_name = match &n.imported {
                    ModuleExportName::Identifier(id) => &id.name,
                    ModuleExportName::String(s) => &s.value,
                };
                if imported_name == &n.local.name {
                    self.write(&n.local.name);
                } else {
                    self.write(imported_name);
                    self.write(" as ");
                    self.write(&n.local.name);
                }
            }
            self.write(" }");
        }
        self.write(" from ");
        self.emit_string_literal(&d.source);
        self.write(";");
    }

    fn emit_export_default(&mut self, d: &ExportDefaultDeclaration) {
        self.write("export default ");
        match &d.declaration {
            ExportDefault::Function(f) => self.emit_function_decl(f),
            ExportDefault::Class(c) => self.emit_class_decl(c),
            ExportDefault::Expression(e) => {
                self.emit_expression(e);
                self.write(";");
            }
        }
    }

    fn emit_export_named(&mut self, d: &ExportNamedDeclaration) {
        self.write("export ");
        if let Some(stmt) = &d.declaration {
            self.emit_statement(stmt);
            return;
        }
        self.write("{ ");
        for (i, s) in d.specifiers.iter().enumerate() {
            if i > 0 {
                self.write(", ");
            }
            let local = export_name(&s.local);
            let exported = export_name(&s.exported);
            if local == exported {
                self.write(local);
            } else {
                self.write(local);
                self.write(" as ");
                self.write(exported);
            }
        }
        self.write(" }");
        if let Some(src) = &d.source {
            self.write(" from ");
            self.emit_string_literal(src);
        }
        self.write(";");
    }

    fn emit_export_all(&mut self, d: &ExportAllDeclaration) {
        self.write("export *");
        if let Some(e) = &d.exported {
            self.write(" as ");
            self.write(export_name(e));
        }
        self.write(" from ");
        self.emit_string_literal(&d.source);
        self.write(";");
    }

    // -- if / for / try / switch ------------------------------------------

    fn emit_if(&mut self, i: &IfStatement) {
        self.write("if (");
        self.emit_expression(&i.test);
        self.write(") ");
        self.emit_statement(&i.consequent);
        if let Some(alt) = &i.alternate {
            self.write(" else ");
            self.emit_statement(alt);
        }
    }

    fn emit_for(&mut self, f: &ForStatement) {
        self.write("for (");
        if let Some(init) = &f.init {
            match init {
                ForInit::Declaration(d) => self.emit_var_decl(d),
                ForInit::Expression(e) => self.emit_expression(e),
            }
        }
        self.write("; ");
        if let Some(t) = &f.test {
            self.emit_expression(t);
        }
        self.write("; ");
        if let Some(u) = &f.update {
            self.emit_expression(u);
        }
        self.write(") ");
        self.emit_statement(&f.body);
    }

    fn emit_for_in(&mut self, f: &ForInStatement) {
        self.write("for (");
        match &f.left {
            ForInit::Declaration(d) => self.emit_var_decl(d),
            ForInit::Expression(e) => self.emit_expression(e),
        }
        self.write(" in ");
        self.emit_expression(&f.right);
        self.write(") ");
        self.emit_statement(&f.body);
    }

    fn emit_for_of(&mut self, f: &ForOfStatement) {
        self.write("for ");
        if f.r#await {
            self.write("await ");
        }
        self.write("(");
        match &f.left {
            ForInit::Declaration(d) => self.emit_var_decl(d),
            ForInit::Expression(e) => self.emit_expression(e),
        }
        self.write(" of ");
        self.emit_expression(&f.right);
        self.write(") ");
        self.emit_statement(&f.body);
    }

    fn emit_try(&mut self, t: &TryStatement) {
        self.write("try ");
        self.emit_block(&t.block);
        if let Some(h) = &t.handler {
            self.write(" catch");
            if let Some(p) = &h.param {
                self.write(" (");
                self.emit_pattern(p);
                self.write(")");
            }
            self.write(" ");
            self.emit_block(&h.body);
        }
        if let Some(f) = &t.finalizer {
            self.write(" finally ");
            self.emit_block(f);
        }
    }

    fn emit_switch(&mut self, s: &SwitchStatement) {
        self.write("switch (");
        self.emit_expression(&s.discriminant);
        self.write(") {");
        self.indent_in();
        for c in &s.cases {
            self.newline();
            if let Some(t) = &c.test {
                self.write("case ");
                self.emit_expression(t);
                self.write(":");
            } else {
                self.write("default:");
            }
            self.indent_in();
            for stmt in &c.consequent {
                self.newline();
                self.emit_statement(stmt);
            }
            self.indent_out();
        }
        self.indent_out();
        self.newline();
        self.write("}");
    }

    // -- expressions -------------------------------------------------------

    fn emit_expression(&mut self, e: &Expression) {
        match e {
            Expression::Identifier(id) => {
                self.map(id.span);
                self.write(&id.name);
            }
            Expression::Literal(lit) => self.emit_literal(lit),
            Expression::Template(t) => self.emit_template(t),
            Expression::Array(a) => self.emit_array(a),
            Expression::Object(o) => self.emit_object(o),
            Expression::Arrow(a) => self.emit_arrow(a),
            Expression::Function(f) => self.emit_function_expr(f),
            Expression::Class(c) => self.emit_class_expr(c),
            Expression::Member(m) => self.emit_member(m),
            Expression::Call(c) => self.emit_call(c),
            Expression::New(n) => self.emit_new(n),
            Expression::Binary(b) => {
                self.emit_paren_if(&b.left, needs_paren_in_binary);
                self.write(" ");
                self.write(b.operator.as_str());
                self.write(" ");
                self.emit_paren_if(&b.right, needs_paren_in_binary);
            }
            Expression::Logical(l) => {
                self.emit_paren_if(&l.left, needs_paren_in_binary);
                self.write(" ");
                self.write(l.operator.as_str());
                self.write(" ");
                self.emit_paren_if(&l.right, needs_paren_in_binary);
            }
            Expression::Assignment(a) => {
                match &a.left {
                    AssignmentTarget::Pattern(p) => self.emit_pattern(p),
                    AssignmentTarget::Expression(e) => self.emit_expression(e),
                }
                self.write(" ");
                self.write(a.operator.as_str());
                self.write(" ");
                self.emit_expression(&a.right);
            }
            Expression::Update(u) => {
                if u.prefix {
                    self.write(u.operator.as_str());
                    self.emit_expression(&u.argument);
                } else {
                    self.emit_expression(&u.argument);
                    self.write(u.operator.as_str());
                }
            }
            Expression::Unary(u) => {
                self.write(u.operator.as_str());
                // typeof / void / delete need a space; sigils don't.
                if matches!(
                    u.operator,
                    UnaryOperator::TypeOf | UnaryOperator::Void | UnaryOperator::Delete
                ) {
                    self.write(" ");
                }
                self.emit_expression(&u.argument);
            }
            Expression::Conditional(c) => {
                self.emit_expression(&c.test);
                self.write(" ? ");
                self.emit_expression(&c.consequent);
                self.write(" : ");
                self.emit_expression(&c.alternate);
            }
            Expression::Sequence(s) => {
                for (i, e) in s.expressions.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    self.emit_expression(e);
                }
            }
            Expression::Spread(s) => {
                self.write("...");
                self.emit_expression(&s.argument);
            }
            Expression::This(_) => self.write("this"),
            Expression::Super(_) => self.write("super"),
            Expression::Yield(y) => {
                self.write("yield");
                if y.delegate {
                    self.write("*");
                }
                if let Some(a) = &y.argument {
                    self.space();
                    self.emit_expression(a);
                }
            }
            Expression::Await(a) => {
                self.write("await ");
                self.emit_expression(&a.argument);
            }
            Expression::Tagged(t) => {
                self.emit_expression(&t.tag);
                self.emit_template(&t.quasi);
            }
            Expression::Paren(p) => {
                self.write("(");
                self.emit_expression(&p.expression);
                self.write(")");
            }
            Expression::Meta(m) => {
                self.write(&m.meta.name);
                self.write(".");
                self.write(&m.property.name);
            }
            Expression::Raw(s) => self.write(s),
        }
    }

    fn emit_member(&mut self, m: &MemberExpression) {
        self.emit_expression(&m.object);
        if m.computed {
            if m.optional {
                self.write("?.");
            }
            self.write("[");
            match &m.property {
                MemberProperty::Expression(e) => self.emit_expression(e),
                MemberProperty::Identifier(id) => self.write(&id.name),
                MemberProperty::Private(p) => {
                    self.write("#");
                    self.write(&p.name);
                }
            }
            self.write("]");
        } else {
            if m.optional {
                self.write("?.");
            } else {
                self.write(".");
            }
            match &m.property {
                MemberProperty::Identifier(id) => {
                    self.map(id.span);
                    self.write(&id.name);
                }
                MemberProperty::Private(p) => {
                    self.write("#");
                    self.write(&p.name);
                }
                MemberProperty::Expression(e) => self.emit_expression(e),
            }
        }
    }

    fn emit_call(&mut self, c: &CallExpression) {
        // Parens needed around callee expressions that bind looser than
        // the call operator. Mirrors the precedence rule esrap inherits
        // from the parser: `await x()` parses as `await (x())`, so the
        // emitter has to force `(await x)()` when an await sits in
        // callee position.
        self.emit_paren_if(&c.callee, needs_paren_in_callee);
        if c.optional {
            self.write("?.");
        }
        self.emit_arg_list(&c.arguments);
    }

    fn emit_new(&mut self, n: &NewExpression) {
        self.write("new ");
        self.emit_paren_if(&n.callee, needs_paren_in_callee);
        self.emit_arg_list(&n.arguments);
    }

    fn emit_arg_list(&mut self, args: &[Argument]) {
        self.write("(");
        if args.is_empty() {
            self.write(")");
            return;
        }
        // CallExpression rule from `esrap/src/languages/ts/index.js:480-540`:
        // only multiline if a NON-LAST argument is itself multiline. The last
        // argument can be multiline without forcing the rest to wrap.
        let render = |emitter: &Self, deeper: bool| -> Vec<Emitter<'a>> {
            args.iter()
                .map(|a| {
                    let mut c = if deeper { emitter.child_indented() } else { emitter.child() };
                    match a {
                        Argument::Expression(e) => c.emit_expression(e),
                        Argument::Spread(s) => {
                            c.write("...");
                            c.emit_expression(&s.argument);
                        }
                    }
                    c
                })
                .collect()
        };
        let probe = render(self, false);
        let n = probe.len();
        let multiline = probe[..n - 1].iter().any(|c| !c.mappings.is_empty());
        // If multiline, re-render with one-deeper indent so child newlines
        // emit at the correct column.
        let children = if multiline { render(self, true) } else { probe };
        if multiline {
            self.indent_in();
            self.newline();
        }
        for (i, child) in children.into_iter().enumerate() {
            if i > 0 {
                if multiline {
                    self.write(",");
                    self.newline();
                } else {
                    self.write(", ");
                }
            }
            self.append_child(child);
        }
        if multiline {
            self.indent_out();
            self.newline();
        }
        self.write(")");
    }

    /// Spawn a sub-emitter sharing `self`'s indent/line_map context. Used
    /// for the look-ahead measure pass in `emit_sequence`-shaped helpers.
    fn child(&self) -> Emitter<'a> {
        Emitter {
            code: String::new(),
            col: 0,
            mappings: Vec::new(),
            current_line: Vec::new(),
            indent: self.indent.clone(),
            indent_unit: self.indent_unit.clone(),
            line_map: self.line_map,
            pending_indent: false,
            comments: self.comments,
            comment_index: self.comment_index,
        }
    }

    /// Like `child()` but pre-indents one level. Used when the helper that
    /// will splice the child's output is about to call `indent_in()` — the
    /// child's interior newlines need to land at the post-indent column.
    fn child_indented(&self) -> Emitter<'a> {
        let mut c = self.child();
        c.indent.push_str(&self.indent_unit);
        c
    }

    /// Splice a child Emitter's output into `self`. Closed mappings from
    /// the child get pushed onto `self.mappings`; the child's open final
    /// line extends `self.current_line`.
    fn append_child(&mut self, child: Emitter<'a>) {
        self.flush_indent();
        for line in child.mappings.into_iter() {
            self.mappings.push(std::mem::take(&mut self.current_line));
            self.current_line = line;
        }
        self.current_line.extend(child.current_line);
        self.code.push_str(&child.code);
        self.col += child.code.chars().count() as u32;
        // (Approximate col update: counts every char, including newlines.
        // For our purposes col is only used in mapping recording which is
        // separately reset on `\n`. Final col matters for subsequent
        // append. Use child.col directly if the child contains no
        // newlines.)
        if !child.code.contains('\n') {
            self.col -= child.code.chars().count() as u32;
            self.col += child.col;
        } else {
            // After at least one newline, our col is whatever's left from
            // the last line — we lost track. Recompute from child.code.
            let last_nl = child.code.rfind('\n').unwrap();
            self.col = child.code[last_nl + 1..].chars().count() as u32;
        }
    }

    /// Decide multiline based on probe, then render the array/object/etc.
    /// sequence. Mirrors `sequence()` at
    /// `esrap/src/languages/ts/index.js:251`.
    fn emit_seq_with<F>(&mut self, sep: &str, pad: bool, mut render: F)
    where
        F: FnMut(&Self, bool) -> Vec<Emitter<'a>>,
    {
        let probe = render(self, false);
        if probe.is_empty() {
            return;
        }
        let mut total: usize = 0;
        let mut any_multiline = false;
        for c in &probe {
            any_multiline |= !c.mappings.is_empty();
            total += c.code.chars().count() + sep.len() + 1;
        }
        total = total.saturating_sub(sep.len() + 1);
        let multiline = any_multiline || total > 60;

        let children = if multiline { render(self, true) } else { probe };

        if multiline {
            self.indent_in();
            self.newline();
        } else if pad {
            self.write(" ");
        }
        let mut prev_multiline = false;
        for (i, child) in children.into_iter().enumerate() {
            let cur_multiline = !child.mappings.is_empty();
            if i > 0 {
                if multiline {
                    self.write(sep);
                    self.newline();
                    // Adjacent multiline children get an extra blank line
                    // between them. Mirrors esrap's `sequence()` rule at
                    // `ts/index.js:293-297`.
                    if prev_multiline && cur_multiline {
                        self.newline();
                    }
                } else {
                    self.write(sep);
                    self.write(" ");
                }
            }
            self.append_child(child);
            prev_multiline = cur_multiline;
        }
        if multiline {
            self.indent_out();
            self.newline();
        } else if pad {
            self.write(" ");
        }
    }

    /// Legacy entry — children already rendered, no re-render on multiline.
    /// Kept for callers that haven't migrated to the `render` closure.
    fn emit_children_seq(&mut self, children: Vec<Emitter<'a>>, sep: &str, pad: bool) {
        if children.is_empty() {
            return;
        }
        let mut total: usize = 0;
        let mut any_multiline = false;
        for c in &children {
            any_multiline |= !c.mappings.is_empty();
            total += c.code.chars().count() + sep.len() + 1;
        }
        total = total.saturating_sub(sep.len() + 1);
        let multiline = any_multiline || total > 60;
        if multiline {
            self.indent_in();
            self.newline();
        } else if pad {
            self.write(" ");
        }
        for (i, child) in children.into_iter().enumerate() {
            if i > 0 {
                if multiline {
                    self.write(sep);
                    self.newline();
                } else {
                    self.write(sep);
                    self.write(" ");
                }
            }
            self.append_child(child);
        }
        if multiline {
            self.indent_out();
            self.newline();
        } else if pad {
            self.write(" ");
        }
    }

    // -- literals ----------------------------------------------------------

    fn emit_literal(&mut self, lit: &Literal) {
        match lit {
            Literal::String(s) => self.emit_string_literal(s),
            Literal::Number(n) => {
                if let Some(raw) = &n.raw {
                    self.write(raw);
                } else if n.value.fract() == 0.0 && n.value.is_finite() {
                    self.write(&(n.value as i64).to_string());
                } else {
                    self.write(&n.value.to_string());
                }
            }
            Literal::Boolean(b) => self.write(if b.value { "true" } else { "false" }),
            Literal::Null(_) => self.write("null"),
            Literal::Regex(r) => {
                self.write("/");
                self.write(&r.pattern);
                self.write("/");
                self.write(&r.flags);
            }
            Literal::BigInt(b) => self.write(&b.raw),
        }
    }

    fn emit_string_literal(&mut self, s: &StringLiteral) {
        if let Some(raw) = &s.raw {
            self.write(raw);
        } else {
            self.flush_indent();
            // Default to single-quoted. Escape backslashes and single quotes.
            self.code.push('\'');
            self.col += 1;
            for ch in s.value.chars() {
                match ch {
                    '\\' => {
                        self.code.push_str("\\\\");
                        self.col += 2;
                    }
                    '\'' => {
                        self.code.push_str("\\'");
                        self.col += 2;
                    }
                    '\n' => {
                        self.code.push_str("\\n");
                        self.col += 2;
                    }
                    '\r' => {
                        self.code.push_str("\\r");
                        self.col += 2;
                    }
                    _ => {
                        self.code.push(ch);
                        self.col += 1;
                    }
                }
            }
            self.code.push('\'');
            self.col += 1;
        }
    }

    fn emit_template(&mut self, t: &TemplateLiteral) {
        self.write("`");
        for (i, q) in t.quasis.iter().enumerate() {
            // Template content goes through verbatim. col tracking is rough
            // (we push the raw chars; the cooked version is what backslashes
            // would translate to). Use raw to match upstream.
            self.flush_indent();
            self.code.push_str(&q.raw);
            for ch in q.raw.bytes() {
                if ch == b'\n' {
                    self.mappings.push(std::mem::take(&mut self.current_line));
                    self.col = 0;
                } else {
                    self.col += 1;
                }
            }
            if !q.tail {
                self.write("${");
                if let Some(e) = t.expressions.get(i) {
                    self.emit_expression(e);
                }
                self.write("}");
            }
        }
        self.write("`");
    }

    // -- arrays / objects --------------------------------------------------

    fn emit_array(&mut self, a: &ArrayExpression) {
        if a.elements.is_empty() {
            self.write("[]");
            return;
        }
        self.write("[");
        self.emit_seq_with(",", false, |this, deeper| {
            a.elements
                .iter()
                .map(|el| {
                    let mut c = if deeper { this.child_indented() } else { this.child() };
                    match el {
                        ArrayElement::Elision => {}
                        ArrayElement::Expression(e) => c.emit_expression(e),
                        ArrayElement::Spread(s) => {
                            c.write("...");
                            c.emit_expression(&s.argument);
                        }
                    }
                    c
                })
                .collect()
        });
        self.write("]");
    }

    fn emit_object(&mut self, o: &ObjectExpression) {
        if o.properties.is_empty() {
            self.write("{}");
            return;
        }
        self.write("{");
        self.emit_seq_with(",", true, |this, deeper| {
            o.properties
                .iter()
                .map(|p| {
                    let mut c = if deeper { this.child_indented() } else { this.child() };
                    match p {
                        ObjectMember::Property(prop) => c.emit_property(prop),
                        ObjectMember::Spread(s) => {
                            c.write("...");
                            c.emit_expression(&s.argument);
                        }
                    }
                    c
                })
                .collect()
        });
        self.write("}");
    }

    fn emit_property(&mut self, p: &Property) {
        match p.kind {
            PropertyKind::Get => {
                self.write("get ");
                self.emit_property_key(&p.key, p.computed);
                if let Expression::Function(f) = &p.value {
                    self.emit_params(&f.params);
                    self.write(" ");
                    self.emit_block(&f.body);
                }
                return;
            }
            PropertyKind::Set => {
                self.write("set ");
                self.emit_property_key(&p.key, p.computed);
                if let Expression::Function(f) = &p.value {
                    self.emit_params(&f.params);
                    self.write(" ");
                    self.emit_block(&f.body);
                }
                return;
            }
            _ => {}
        }
        if p.method {
            if let Expression::Function(f) = &p.value {
                if f.r#async {
                    self.write("async ");
                }
                if f.generator {
                    self.write("*");
                }
                self.emit_property_key(&p.key, p.computed);
                self.emit_params(&f.params);
                self.write(" ");
                self.emit_block(&f.body);
                return;
            }
        }
        // Auto-detect shorthand: `{ x: x }` → `{ x }`. esrap upstream emits
        // shorthand whenever the key is an Identifier and the value is an
        // Identifier with the same name. Our builders rarely set the
        // `shorthand` flag, so we mirror the runtime detection here.
        if let (PropertyKey::Identifier(k), Expression::Identifier(v)) = (&p.key, &p.value) {
            if !p.computed && k.name == v.name {
                self.emit_property_key(&p.key, p.computed);
                return;
            }
        }
        if p.shorthand {
            self.emit_property_key(&p.key, p.computed);
            return;
        }
        self.emit_property_key(&p.key, p.computed);
        self.write(": ");
        self.emit_expression(&p.value);
    }

    fn emit_property_key(&mut self, k: &PropertyKey, computed: bool) {
        if computed {
            self.write("[");
            match k {
                PropertyKey::Identifier(id) => self.write(&id.name),
                PropertyKey::Literal(l) => self.emit_literal(l),
                PropertyKey::Expression(e) => self.emit_expression(e),
                PropertyKey::Private(p) => {
                    self.write("#");
                    self.write(&p.name);
                }
            }
            self.write("]");
        } else {
            match k {
                PropertyKey::Identifier(id) => self.write(&id.name),
                PropertyKey::Literal(l) => self.emit_literal(l),
                PropertyKey::Expression(e) => self.emit_expression(e),
                PropertyKey::Private(p) => {
                    self.write("#");
                    self.write(&p.name);
                }
            }
        }
    }

    // -- patterns ----------------------------------------------------------

    fn emit_pattern(&mut self, p: &Pattern) {
        match p {
            Pattern::Identifier(id) => {
                self.map(id.span);
                self.write(&id.name);
            }
            Pattern::Array(a) => {
                self.write("[");
                for (i, el) in a.elements.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    if let Some(p) = el {
                        self.emit_pattern(p);
                    }
                }
                self.write("]");
            }
            Pattern::Object(o) => {
                if o.properties.is_empty() {
                    self.write("{}");
                    return;
                }
                self.write("{ ");
                for (i, m) in o.properties.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    match m {
                        ObjectPatternMember::Property(p) => {
                            if p.shorthand {
                                self.emit_pattern(&p.value);
                            } else {
                                self.emit_property_key(&p.key, p.computed);
                                self.write(": ");
                                self.emit_pattern(&p.value);
                            }
                        }
                        ObjectPatternMember::Rest(r) => {
                            self.write("...");
                            self.emit_pattern(&r.argument);
                        }
                    }
                }
                self.write(" }");
            }
            Pattern::Rest(r) => {
                self.write("...");
                self.emit_pattern(&r.argument);
            }
            Pattern::Assignment(a) => {
                self.emit_pattern(&a.left);
                self.write(" = ");
                self.emit_expression(&a.right);
            }
            Pattern::Member(m) => self.emit_member(m),
        }
    }
}

// --- helpers --------------------------------------------------------------

fn export_name(n: &ModuleExportName) -> &str {
    match n {
        ModuleExportName::Identifier(id) => &id.name,
        ModuleExportName::String(s) => &s.value,
    }
}

/// Per-variant tag used to compare statements for the "same-type cluster"
/// rule (esrap's `prev_type === child.type` check). Variable declarations
/// of different `kind` (var/let/const) are kept distinct so `let` and
/// `var` clustered together still emit a margin between groups.
/// True if an expression needs to be wrapped in parens when it appears as
/// a child of a Binary / Logical expression. Assignments and sequences
/// bind looser than `===`/`<`/`+` etc., so they need parens to avoid
/// flipping the parse.
fn needs_paren_in_binary(e: &Expression) -> bool {
    matches!(e, Expression::Assignment(_) | Expression::Sequence(_))
}

/// True if an expression needs to be wrapped in parens when it sits in
/// callee position of a CallExpression / NewExpression. `await x()` parses
/// as `await (x())`, so to call the awaited value the await must be
/// parenthesized.
fn needs_paren_in_callee(e: &Expression) -> bool {
    matches!(
        e,
        Expression::Await(_)
            | Expression::Yield(_)
            | Expression::Arrow(_)
            | Expression::Assignment(_)
            | Expression::Sequence(_)
            | Expression::Conditional(_)
            | Expression::Logical(_)
            | Expression::Binary(_)
            | Expression::Unary(_)
            | Expression::Update(_)
    )
}

impl<'a> Emitter<'a> {
    fn emit_paren_if(&mut self, e: &Expression, needs: fn(&Expression) -> bool) {
        if needs(e) {
            self.write("(");
            self.emit_expression(e);
            self.write(")");
        } else {
            self.emit_expression(e);
        }
    }
}

fn class_member_tag(m: &ClassMember) -> u32 {
    match m {
        ClassMember::Method(md) => match md.kind {
            MethodKind::Constructor => 1,
            MethodKind::Method => 2,
            MethodKind::Get => 3,
            MethodKind::Set => 4,
        },
        ClassMember::Property(_) => 5,
        ClassMember::StaticBlock(_) => 6,
    }
}

fn stmt_type_tag(s: &Statement) -> u32 {
    match s {
        Statement::Import(_) => 1,
        // Upstream treats any VariableDeclaration as the same `node.type`
        // regardless of `var`/`let`/`const` — that's a `kind` field on the
        // declaration, not the AST node type.
        Statement::Variable(_) => 2,
        Statement::Function(_) => 3,
        Statement::Class(_) => 4,
        Statement::Expression(_) => 5,
        Statement::Return(_) => 6,
        Statement::ExportNamed(_) => 7,
        Statement::ExportDefault(_) => 8,
        Statement::ExportAll(_) => 9,
        Statement::Block(_) => 10,
        Statement::If(_) => 11,
        Statement::For(_) => 12,
        Statement::ForIn(_) => 13,
        Statement::ForOf(_) => 14,
        Statement::While(_) => 15,
        Statement::DoWhile(_) => 16,
        Statement::Break(_) => 17,
        Statement::Continue(_) => 18,
        Statement::Empty(_) => 19,
        Statement::Debugger(_) => 20,
        Statement::Throw(_) => 21,
        Statement::Try(_) => 22,
        Statement::Labeled(_) => 23,
        Statement::Switch(_) => 24,
        Statement::With(_) => 25,
        Statement::Raw(_) => 26,
    }
}

// --- smoke tests ----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn id(name: &str) -> Expression {
        Expression::Identifier(Identifier {
            name: name.to_string(),
            span: Span::ZERO,
        })
    }

    #[test]
    fn smoke_identifier() {
        let prog = Program {
            source_type: SourceType::Module,
            body: vec![Statement::Expression(Box::new(ExpressionStatement {
                expression: id("foo"),
                span: Span::ZERO,
            }))],
            span: Span::ZERO,
        };
        let r = print_typed(&prog, &TypedPrintOptions::default());
        assert_eq!(r.code, "foo;");
    }

    #[test]
    fn smoke_call() {
        let call = Expression::Call(Box::new(CallExpression {
            callee: id("foo"),
            arguments: vec![Argument::Expression(id("bar"))],
            optional: false,
            span: Span::ZERO,
        }));
        let prog = Program {
            source_type: SourceType::Module,
            body: vec![Statement::Expression(Box::new(ExpressionStatement {
                expression: call,
                span: Span::ZERO,
            }))],
            span: Span::ZERO,
        };
        let r = print_typed(&prog, &TypedPrintOptions::default());
        assert_eq!(r.code, "foo(bar);");
    }

    #[test]
    fn smoke_var_decl() {
        let prog = Program {
            source_type: SourceType::Module,
            body: vec![Statement::Variable(Box::new(VariableDeclaration {
                kind: VariableKind::Var,
                declarations: vec![VariableDeclarator {
                    id: Pattern::Identifier(Identifier {
                        name: "x".to_string(),
                        span: Span::ZERO,
                    }),
                    init: Some(Expression::Literal(Box::new(Literal::Number(
                        NumberLiteral {
                            value: 42.0,
                            raw: None,
                            span: Span::ZERO,
                        },
                    )))),
                    span: Span::ZERO,
                }],
                span: Span::ZERO,
            }))],
            span: Span::ZERO,
        };
        let r = print_typed(&prog, &TypedPrintOptions::default());
        assert_eq!(r.code, "var x = 42;");
    }

    #[test]
    fn smoke_import_namespace() {
        let prog = Program {
            source_type: SourceType::Module,
            body: vec![Statement::Import(Box::new(ImportDeclaration {
                specifiers: vec![ImportSpecifierKind::Namespace(ImportNamespaceSpecifier {
                    local: Identifier {
                        name: "$".to_string(),
                        span: Span::ZERO,
                    },
                    span: Span::ZERO,
                })],
                source: StringLiteral {
                    value: "svelte/internal/client".to_string(),
                    raw: None,
                    span: Span::ZERO,
                },
                span: Span::ZERO,
            }))],
            span: Span::ZERO,
        };
        let r = print_typed(&prog, &TypedPrintOptions::default());
        assert_eq!(r.code, "import * as $ from 'svelte/internal/client';");
    }
}
