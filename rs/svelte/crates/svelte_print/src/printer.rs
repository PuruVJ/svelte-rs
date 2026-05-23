//! Recursive Svelte AST printer.
//!
//! Mirrors the visitors at `packages/svelte/src/compiler/print/index.js`.
//! Output is valid Svelte source; formatting is normalised (tabs for indent,
//! 50-column line-break threshold for attribute wrap, etc.).

use svelte_ast::attributes::{
    AnimateDirective, Attribute, AttributeValue, AttributeValuePart, BindDirective,
    ClassDirective, ElementAttribute, LetDirective, OnDirective, SpreadAttribute, StyleDirective,
    TransitionDirective, UseDirective,
};
use svelte_ast::blocks::{AwaitBlock, EachBlock, IfBlock, KeyBlock, SnippetBlock};
use svelte_ast::elements::{
    Component, RegularElement, SlotElement, SpecialElement, SvelteComponent, SvelteElement,
    TitleElement,
};
use svelte_ast::fragment::{Comment, Fragment, FragmentChild, Text};
use svelte_ast::root::{Root, Script, ScriptContext};
use svelte_ast::tags::{
    AttachTag, ConstTag, DebugTag, ExpressionTag, HtmlTag, RenderTag,
};

use svelte_codegen_js::{
    print_expression_str, print_pattern_str, print_statements_str,
    print_statements_str_with_comments, TypedComment, TypedCommentKind,
};

#[derive(Debug, Clone, Default)]
pub struct PrintOptions {
    /// Indentation unit; default `"\t"`.
    pub indent: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PrintResult {
    pub code: String,
}

/// Print a `Root` AST node back to a `.svelte` source string.
pub fn print(root: &Root<'_>, opts: PrintOptions) -> PrintResult {
    let mut p = Printer::new(opts);
    p.root_comments = root_comments_to_typed(root);
    p.emit_root(root);
    // Ensure trailing newline matches upstream's output convention.
    if !p.out.ends_with('\n') {
        p.out.push('\n');
    }
    PrintResult { code: p.out }
}

fn root_comments_to_typed(root: &Root<'_>) -> Vec<TypedComment> {
    root.comments
        .iter()
        .map(|c| TypedComment {
            kind: match c.kind {
                svelte_ast::root::JsCommentKind::Line => TypedCommentKind::Line,
                svelte_ast::root::JsCommentKind::Block => TypedCommentKind::Block,
            },
            value: c.value.clone(),
            start: c.start,
            end: c.end,
        })
        .collect()
}

// ---- internals ----------------------------------------------------------

const LINE_BREAK_THRESHOLD: usize = 50;

/// True when an empty-body element should self-close (`<X />`). Mirrors
/// upstream's `is_self_closing` check: void HTML elements OR Components
/// with no body. `svelte:options` is also always self-closing (handled
/// at the Root level by upstream; we match it here for unification).
fn empty_self_closes(name: &str) -> bool {
    if is_void_element(name) {
        return true;
    }
    if name == "svelte:options" {
        return true;
    }
    let first = name.chars().next().unwrap_or(' ');
    if first.is_ascii_uppercase() {
        return true;
    }
    if name.contains('.') {
        return true;
    }
    false
}

fn is_void_element(name: &str) -> bool {
    matches!(
        name,
        "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input"
        | "link" | "meta" | "param" | "source" | "track" | "wbr"
        | "circle" | "ellipse" | "line" | "path" | "polygon" | "polyline" | "rect"
        | "stop" | "use"
    )
}

struct Printer {
    out: String,
    indent: String,
    indent_unit: String,
    /// Mirrors esrap's `Context.multiline` — set when `newline()` /
    /// `line()` / margin-emitter explicitly broke a line. Content that
    /// contains `\n` characters via `write` does NOT trigger this (e.g.
    /// a multi-line comment's data).
    multiline: bool,
    /// All JS-side comments from the source. Threaded into the JS
    /// codegen when printing script bodies so block-statement bodies
    /// with only comments (`(node) => { /* … */ }`) round-trip.
    root_comments: Vec<TypedComment>,
}

impl Printer {
    fn new(opts: PrintOptions) -> Self {
        Self {
            out: String::with_capacity(1024),
            indent: String::new(),
            indent_unit: opts.indent.unwrap_or_else(|| "\t".to_string()),
            multiline: false,
            root_comments: Vec::new(),
        }
    }

    fn write(&mut self, s: &str) {
        self.out.push_str(s);
    }

    fn newline(&mut self) {
        self.out.push('\n');
        self.out.push_str(&self.indent);
        self.multiline = true;
    }

    /// Drop any trailing whitespace then push a single `\n` — used between
    /// blocks where we want a clean line break regardless of pending indent.
    fn line(&mut self) {
        while self.out.ends_with(' ') || self.out.ends_with('\t') {
            self.out.pop();
        }
        self.out.push('\n');
        self.out.push_str(&self.indent);
        self.multiline = true;
    }

    fn blank_line(&mut self) {
        self.line();
        // Emit an extra bare newline (no indent) so we get a margin.
        self.out.push('\n');
        self.out.push_str(&self.indent);
    }

    fn indent_in(&mut self) {
        self.indent.push_str(&self.indent_unit);
    }

    fn indent_out(&mut self) {
        let n = self.indent_unit.len();
        let len = self.indent.len().saturating_sub(n);
        self.indent.truncate(len);
    }

    // ---- root ---------------------------------------------------------

    fn emit_root(&mut self, root: &Root<'_>) {
        // Top-level fragment order: module script first, then instance
        // script, then template fragment, then style. Upstream `print()`
        // walks `root.fragment.nodes` in source order, but module/instance/
        // style live OUTSIDE the fragment in our AST — emit them at the
        // boundary points that match upstream's typical input layout.
        if let Some(s) = root.module.as_ref() {
            self.emit_script(s);
            self.line();
            self.line();
        }
        if let Some(s) = root.instance.as_ref() {
            self.emit_script(s);
            self.line();
            self.line();
        }
        self.emit_fragment(&root.fragment, false);
        if let Some(css) = root.css.as_ref() {
            self.line();
            self.line();
            self.emit_style(css);
        }
    }

    // ---- script -------------------------------------------------------

    fn emit_script(&mut self, s: &Script) {
        self.write("<script");
        let has_module_attr = s
            .attributes
            .iter()
            .any(|a| a.name == "module");
        // Only emit the bare `module` token if the parser flagged the
        // script as module-context but didn't include it as an attribute
        // (the AST stores `module` either way depending on shape).
        if matches!(s.context, ScriptContext::Module) && !has_module_attr {
            self.write(" module");
        }
        for a in &s.attributes {
            self.write(" ");
            self.write(&attribute_str(a));
        }
        self.write(">");
        self.indent_in();
        self.out.push('\n');
        self.out.push_str(&self.indent);
        // Filter root_comments to those within the script's content span
        // (so the JS codegen flushes only comments local to this script).
        let s_start = s.content.span.start;
        let s_end = s.content.span.end;
        let script_comments: Vec<TypedComment> = self
            .root_comments
            .iter()
            .filter(|c| c.start >= s_start && c.start < s_end)
            .cloned()
            .collect();
        let body = print_statements_str_with_comments(&s.content.body, &script_comments);
        let body_trimmed = body.trim_end_matches('\n');
        // Re-indent each line of the body to our current indent level.
        // Blank lines stay blank (no trailing indent).
        let lines: Vec<&str> = body_trimmed.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                self.out.push('\n');
                if !line.is_empty() {
                    self.out.push_str(&self.indent);
                }
            }
            if !line.is_empty() {
                self.write(line);
            }
        }
        self.indent_out();
        self.out.push('\n');
        self.out.push_str(&self.indent);
        self.write("</script>");
    }

    // ---- style --------------------------------------------------------

    fn emit_style(&mut self, css: &svelte_ast::css::StyleSheet) {
        self.write("<style");
        for a in &css.attributes {
            self.write(" ");
            let s = self.attribute_to_string(a);
            self.write(&s);
        }
        self.write(">");
        if css.children.is_empty() {
            self.write("</style>");
            return;
        }
        self.indent_in();
        for (i, child) in css.children.iter().enumerate() {
            if i == 0 {
                self.newline();
            } else {
                self.out.push('\n');
                self.out.push('\n');
                self.out.push_str(&self.indent);
            }
            self.emit_css_child(child);
        }
        self.indent_out();
        self.newline();
        self.write("</style>");
    }

    fn emit_css_child(&mut self, child: &svelte_ast::css::StyleSheetChild) {
        match child {
            svelte_ast::css::StyleSheetChild::Rule(r) => self.emit_css_rule(r),
            svelte_ast::css::StyleSheetChild::Atrule(a) => self.emit_css_atrule(a),
        }
    }

    fn emit_css_rule(&mut self, r: &svelte_ast::css::Rule) {
        self.emit_css_selector_list(&r.prelude);
        self.write(" ");
        self.emit_css_block(&r.block);
    }

    fn emit_css_atrule(&mut self, a: &svelte_ast::css::Atrule) {
        self.write("@");
        self.write(&a.name);
        if !a.prelude.is_empty() {
            self.write(" ");
            self.write(&a.prelude);
        }
        if let Some(b) = &a.block {
            self.write(" ");
            self.emit_css_block(b);
        } else {
            self.write(";");
        }
    }

    fn emit_css_block(&mut self, b: &svelte_ast::css::Block) {
        self.write("{");
        if b.children.is_empty() {
            self.write("}");
            return;
        }
        self.indent_in();
        for child in &b.children {
            self.newline();
            self.emit_css_block_child(child);
        }
        self.indent_out();
        self.newline();
        self.write("}");
    }

    fn emit_css_block_child(&mut self, c: &svelte_ast::css::BlockChild) {
        match c {
            svelte_ast::css::BlockChild::Declaration(d) => {
                self.write(&d.property);
                self.write(": ");
                self.write(&d.value);
                self.write(";");
            }
            svelte_ast::css::BlockChild::Rule(r) => self.emit_css_rule(r),
            svelte_ast::css::BlockChild::Atrule(a) => self.emit_css_atrule(a),
        }
    }

    fn emit_css_selector_list(&mut self, list: &svelte_ast::css::SelectorList) {
        for (i, sel) in list.children.iter().enumerate() {
            if i > 0 {
                self.write(",");
                self.newline();
            }
            self.emit_css_complex_selector(sel);
        }
    }

    fn emit_css_complex_selector(&mut self, sel: &svelte_ast::css::ComplexSelector) {
        for rel in &sel.children {
            self.emit_css_relative_selector(rel);
        }
    }

    fn emit_css_relative_selector(&mut self, rel: &svelte_ast::css::RelativeSelector) {
        if let Some(comb) = &rel.combinator {
            if comb.name == " " {
                self.write(" ");
            } else {
                self.write(" ");
                self.write(&comb.name);
                self.write(" ");
            }
        }
        for s in &rel.selectors {
            self.emit_css_simple_selector(s);
        }
    }

    fn emit_css_simple_selector(&mut self, s: &svelte_ast::css::SimpleSelector) {
        use svelte_ast::css::SimpleSelector::*;
        match s {
            TypeSelector(t) => self.write(&t.name),
            IdSelector(i) => {
                self.write("#");
                self.write(&i.name);
            }
            ClassSelector(c) => {
                self.write(".");
                self.write(&c.name);
            }
            AttributeSelector(a) => {
                self.write("[");
                self.write(&a.name);
                if let Some(matcher) = &a.matcher {
                    self.write(matcher);
                    if let Some(v) = &a.value {
                        self.write("\"");
                        self.write(v);
                        self.write("\"");
                    }
                    if let Some(f) = &a.flags {
                        self.write(" ");
                        self.write(f);
                    }
                }
                self.write("]");
            }
            PseudoElementSelector(p) => {
                self.write("::");
                self.write(&p.name);
            }
            PseudoClassSelector(p) => {
                self.write(":");
                self.write(&p.name);
                if let Some(args) = &p.args {
                    self.write("(");
                    for (i, sel) in args.children.iter().enumerate() {
                        if i > 0 {
                            self.write(", ");
                        }
                        self.emit_css_complex_selector(sel);
                    }
                    self.write(")");
                }
            }
            Percentage(p) => {
                self.write(&p.value);
                self.write("%");
            }
            Nth(n) => self.write(&n.value),
            NestingSelector(_) => self.write("&"),
        }
    }

    // ---- fragment -----------------------------------------------------

    /// Emit a fragment's children using upstream's clean-and-sequence rules.
    /// Mirrors `Fragment` visitor at packages/svelte/src/compiler/print/index.js.
    ///
    /// Text nodes get whitespace collapsed to single spaces, with leading/
    /// trailing trims on first/last. Block-shaped elements flush the current
    /// run into its own sequence so they get newline-separated. Inline runs
    /// are emitted on a single line. If any sub-emission was multiline OR
    /// total width exceeds 50, sequences become newline-separated.
    fn emit_fragment(&mut self, f: &Fragment<'_>, _inline: bool) {
        let sequences = clean_and_sequence(&f.nodes);

        // Render each sequence into a probe so we can measure multiline
        // state + width. Multiline reflects only explicit newline() calls
        // during the probe (matches esrap's Context.multiline).
        let mut rendered: Vec<(String, bool)> = Vec::with_capacity(sequences.len());
        let mut width = 0usize;
        let mut any_multiline = false;
        for seq in &sequences {
            let (probe, ml) = self.render_sequence(seq);
            width += probe.chars().count();
            any_multiline |= ml;
            rendered.push((probe, ml));
        }
        let multiline = any_multiline || width > LINE_BREAK_THRESHOLD;

        for i in 0..rendered.len() {
            let prev_multiline = if i > 0 { rendered[i - 1].1 } else { false };
            let cur_multiline = rendered[i].1;
            if i > 0 {
                if prev_multiline || cur_multiline {
                    // Margin between multiline sequences.
                    self.line();
                    self.out.push('\n');
                    self.out.push_str(&self.indent);
                } else if multiline {
                    self.line();
                }
            }
            self.write(&rendered[i].0);
            if rendered[i].1 {
                self.multiline = true;
            }
        }
    }

    fn render_sequence(&self, seq: &[SeqItem<'_>]) -> (String, bool) {
        // Render the sequence in a sub-printer that inherits current indent.
        let mut sub = Printer {
            out: String::with_capacity(64),
            indent: self.indent.clone(),
            indent_unit: self.indent_unit.clone(),
            multiline: false,
            root_comments: self.root_comments.clone(),
        };
        for item in seq {
            match item {
                SeqItem::Node(n) => sub.emit_node(n),
                SeqItem::Text(s) => sub.write(s),
            }
        }
        (sub.out, sub.multiline)
    }

    fn last_was_newline(&self) -> bool {
        self.out.ends_with('\n') || self.out.is_empty()
    }

    fn emit_node(&mut self, n: &FragmentChild) {
        match n {
            FragmentChild::Text(t) => self.emit_text(t),
            FragmentChild::Comment(c) => self.emit_comment(c),
            FragmentChild::ExpressionTag(t) => self.emit_expression_tag(t),
            FragmentChild::HtmlTag(t) => self.emit_html_tag(t),
            FragmentChild::ConstTag(t) => self.emit_const_tag(t),
            FragmentChild::DebugTag(t) => self.emit_debug_tag(t),
            FragmentChild::RenderTag(t) => self.emit_render_tag(t),
            FragmentChild::AttachTag(_) => {
                // AttachTag only appears as an ElementAttribute, not as a
                // fragment child — but the AST allows it. No-op for safety.
            }
            FragmentChild::RegularElement(e) => self.emit_regular_element(e),
            FragmentChild::Component(c) => self.emit_component(c),
            FragmentChild::SlotElement(e) => self.emit_slot_element(e),
            FragmentChild::TitleElement(e) => self.emit_title_element(e),
            FragmentChild::SvelteElement(e) => self.emit_svelte_element(e),
            FragmentChild::SvelteComponent(e) => self.emit_svelte_component(e),
            FragmentChild::SvelteSelf(e) => self.emit_special(e, "svelte:self"),
            FragmentChild::SvelteFragment(e) => self.emit_special(e, "svelte:fragment"),
            FragmentChild::SvelteBoundary(e) => self.emit_special(e, "svelte:boundary"),
            FragmentChild::SvelteBody(e) => self.emit_special(e, "svelte:body"),
            FragmentChild::SvelteHead(e) => self.emit_special(e, "svelte:head"),
            FragmentChild::SvelteDocument(e) => self.emit_special(e, "svelte:document"),
            FragmentChild::SvelteWindow(e) => self.emit_special(e, "svelte:window"),
            FragmentChild::SvelteOptions(e) => self.emit_special(e, "svelte:options"),
            FragmentChild::IfBlock(b) => self.emit_if_block(b, false),
            FragmentChild::EachBlock(b) => self.emit_each_block(b),
            FragmentChild::AwaitBlock(b) => self.emit_await_block(b),
            FragmentChild::KeyBlock(b) => self.emit_key_block(b),
            FragmentChild::SnippetBlock(b) => self.emit_snippet_block(b),
        }
    }

    fn emit_text(&mut self, t: &Text) {
        self.write(&t.raw);
    }

    fn emit_comment(&mut self, c: &Comment) {
        self.write("<!--");
        self.write(&c.data);
        self.write("-->");
    }

    fn emit_expression_tag(&mut self, t: &ExpressionTag) {
        self.write("{");
        self.write(&print_expression_str(&t.expression));
        self.write("}");
    }

    fn emit_html_tag(&mut self, t: &HtmlTag) {
        self.write("{@html ");
        self.write(&print_expression_str(&t.expression));
        self.write("}");
    }

    fn emit_render_tag(&mut self, t: &RenderTag) {
        self.write("{@render ");
        self.write(&print_expression_str(&t.expression));
        self.write("}");
    }

    fn emit_const_tag(&mut self, t: &ConstTag) {
        self.write("{@const ");
        // ConstTag's declaration is a `const X = …` — emit the declarator
        // body without the leading `const ` (upstream wraps it differently).
        let stmts = vec![svelte_js_ast::Statement::Variable(Box::new(t.declaration.clone()))];
        let s = print_statements_str(&stmts);
        // Strip leading "const " (or "let "/"var " — defensive).
        let s = s
            .trim_end_matches(';')
            .trim_end_matches('\n')
            .trim_start()
            .to_string();
        let s = s
            .strip_prefix("const ")
            .or_else(|| s.strip_prefix("let "))
            .or_else(|| s.strip_prefix("var "))
            .unwrap_or(&s);
        self.write(s);
        self.write("}");
    }

    fn emit_debug_tag(&mut self, t: &DebugTag) {
        self.write("{@debug");
        for (i, id) in t.identifiers.iter().enumerate() {
            if i == 0 {
                self.write(" ");
            } else {
                self.write(", ");
            }
            self.write(&id.name);
        }
        self.write("}");
    }

    // ---- elements -----------------------------------------------------

    fn emit_regular_element(&mut self, el: &RegularElement) {
        self.emit_element_open(&el.name, &el.attributes, &el.fragment, is_void_element(&el.name));
    }

    fn emit_component(&mut self, c: &Component) {
        self.emit_element_open(&c.name, &c.attributes, &c.fragment, false);
    }

    fn emit_slot_element(&mut self, el: &SlotElement) {
        self.emit_element_open("slot", &el.attributes, &el.fragment, false);
    }

    fn emit_title_element(&mut self, el: &TitleElement) {
        self.emit_element_open("title", &el.attributes, &el.fragment, false);
    }

    fn emit_svelte_element(&mut self, el: &SvelteElement<'_>) {
        // SvelteElement: self-close when empty body; otherwise always use
        // block-style body (upstream calls `block(context, fragment)`
        // without the `allow_inline` flag — see print/index.js:858).
        let mut attr_strs: Vec<String> = Vec::with_capacity(el.attributes.len() + 1);
        attr_strs.push(format!("this={{{}}}", print_expression_str(&el.tag)));
        for a in &el.attributes {
            attr_strs.push(self.attribute_to_string(a));
        }
        if el.fragment.nodes.is_empty() {
            self.emit_element_open_with_attr_strs("svelte:element", &attr_strs, &el.fragment, true);
            return;
        }
        // Open tag (attrs may wrap).
        let inline_len = "svelte:element".len() + 2
            + attr_strs.iter().map(|s| s.len() + 1).sum::<usize>();
        let wrap_attrs = inline_len > LINE_BREAK_THRESHOLD && !attr_strs.is_empty();
        self.write("<svelte:element");
        if wrap_attrs {
            self.indent_in();
            for s in &attr_strs {
                self.newline();
                self.write(s);
            }
            self.indent_out();
            self.newline();
        } else {
            for s in &attr_strs {
                self.write(" ");
                self.write(s);
            }
        }
        self.write(">");
        self.emit_block_body(&el.fragment);
        self.write("</svelte:element>");
    }

    fn emit_svelte_component(&mut self, el: &SvelteComponent<'_>) {
        let mut attr_strs: Vec<String> = Vec::with_capacity(el.attributes.len() + 1);
        attr_strs.push(format!(
            "this={{{}}}",
            print_expression_str(&el.expression)
        ));
        for a in &el.attributes {
            attr_strs.push(self.attribute_to_string(a));
        }
        let has_body = !el.fragment.nodes.is_empty();
        self.emit_element_open_with_attr_strs(
            "svelte:component",
            &attr_strs,
            &el.fragment,
            !has_body,
        );
    }

    fn emit_special(&mut self, el: &SpecialElement, name: &str) {
        // `is_void` controls the `<X />` short form. Only `svelte:options`
        // takes it; svelte:window/document/body etc. use explicit
        // `<X></X>` even when empty (matches upstream's print output).
        let is_void = name == "svelte:options" && el.fragment.nodes.is_empty();
        self.emit_element_open(name, &el.attributes, &el.fragment, is_void);
    }

    fn emit_element_open(
        &mut self,
        name: &str,
        attrs: &[ElementAttribute<'_>],
        fragment: &Fragment<'_>,
        is_void: bool,
    ) {
        let attr_strs: Vec<String> = attrs.iter().map(|a| self.attribute_to_string(a)).collect();
        self.emit_element_open_with_attr_strs(name, &attr_strs, fragment, is_void);
    }

    fn emit_element_open_with_attr_strs(
        &mut self,
        name: &str,
        attr_strs: &[String],
        fragment: &Fragment<'_>,
        is_void: bool,
    ) {
        let inline_len = name.len() + 2 // < + >
            + attr_strs.iter().map(|s| s.len() + 1).sum::<usize>();
        // svelte:options always stays inline — upstream's Root visitor
        // hand-writes it without going through base_element's wrap logic.
        let wrap_attrs = name != "svelte:options"
            && inline_len > LINE_BREAK_THRESHOLD
            && !attr_strs.is_empty();

        let is_doctype = name.eq_ignore_ascii_case("!doctype");
        self.write("<");
        self.write(name);
        if wrap_attrs {
            self.indent_in();
            for s in attr_strs {
                self.newline();
                self.write(s);
            }
            self.indent_out();
            self.newline();
        } else {
            for s in attr_strs {
                self.write(" ");
                self.write(s);
            }
        }
        // Doctype: just close with `>`, no body / close tag.
        if is_doctype {
            self.write(">");
            return;
        }
        // Empty body — self-close (`<X />` or `<X attrs />`) ONLY for
        // void HTML, Components, `svelte:options`, namespaced (`Foo.Bar`),
        // and uppercase-leading names. svelte:window/document/head/etc.
        // use explicit `<X></X>` close even when empty.
        let has_children = !fragment.nodes.is_empty();
        if !has_children {
            let self_close = is_void || empty_self_closes(name);
            if self_close {
                if wrap_attrs {
                    self.write("/>");
                } else {
                    self.write(" />");
                }
                return;
            }
            self.write("></");
            self.write(name);
            self.write(">");
            return;
        }

        self.write(">");
        self.emit_element_body(fragment);
        self.write("</");
        self.write(name);
        self.write(">");
    }

    /// Emit a block body (consequent, each-body, snippet-body, etc.) —
    /// always indented + newline-separated using clean_and_sequence.
    fn emit_block_body(&mut self, f: &Fragment<'_>) {
        let sequences = clean_and_sequence(&f.nodes);
        if sequences.is_empty() {
            return;
        }
        self.indent_in();
        let mut rendered: Vec<(String, bool)> = Vec::with_capacity(sequences.len());
        for seq in &sequences {
            rendered.push(self.render_sequence(seq));
        }
        for (i, (s, m)) in rendered.iter().enumerate() {
            let prev_multiline = if i > 0 { rendered[i - 1].1 } else { false };
            if i == 0 {
                self.newline();
            } else if prev_multiline || *m {
                self.out.push('\n');
                self.out.push('\n');
                self.out.push_str(&self.indent);
            } else {
                self.newline();
            }
            self.write(s);
        }
        self.indent_out();
        self.newline();
    }

    /// Emit the body of an element, deciding inline vs block based on the
    /// same clean-and-sequence rules as the top-level Fragment visitor.
    fn emit_element_body(&mut self, f: &Fragment<'_>) {
        let sequences = clean_and_sequence(&f.nodes);
        if sequences.is_empty() {
            return;
        }

        // Probe each sequence at the current indent + 1 to detect multiline.
        self.indent_in();
        let mut rendered: Vec<(String, bool)> = Vec::with_capacity(sequences.len());
        let mut width = 0usize;
        let mut any_multiline = false;
        for seq in &sequences {
            let (probe, ml) = self.render_sequence(seq);
            width += probe.chars().count();
            any_multiline |= ml;
            rendered.push((probe, ml));
        }
        let multiline = any_multiline || width > LINE_BREAK_THRESHOLD;
        self.indent_out();

        if !multiline {
            // Inline: render each sequence with no inter-sequence break.
            for (s, _) in &rendered {
                self.write(s);
            }
            return;
        }
        // Block: indented child lines.
        self.indent_in();
        for (i, (s, m)) in rendered.iter().enumerate() {
            let prev_multiline = if i > 0 { rendered[i - 1].1 } else { false };
            if i == 0 {
                self.newline();
            } else if prev_multiline || *m {
                self.out.push('\n');
                self.out.push('\n');
                self.out.push_str(&self.indent);
            } else {
                self.newline();
            }
            self.write(s);
        }
        self.indent_out();
        self.newline();
    }

    // ---- attributes ---------------------------------------------------

    fn emit_attribute(&mut self, attr: &ElementAttribute) {
        self.write(&self.attribute_to_string(attr));
    }

    fn attribute_to_string(&self, attr: &ElementAttribute) -> String {
        match attr {
            ElementAttribute::Attribute(a) => attribute_str(a),
            ElementAttribute::SpreadAttribute(s) => spread_attribute_str(s),
            ElementAttribute::AnimateDirective(d) => animate_str(d),
            ElementAttribute::BindDirective(d) => bind_str(d),
            ElementAttribute::ClassDirective(d) => class_str(d),
            ElementAttribute::LetDirective(d) => let_str(d),
            ElementAttribute::OnDirective(d) => on_str(d),
            ElementAttribute::StyleDirective(d) => style_directive_str(d),
            ElementAttribute::TransitionDirective(d) => transition_str(d),
            ElementAttribute::UseDirective(d) => use_str(d),
            ElementAttribute::AttachTag(t) => attach_str(t),
        }
    }

    // ---- blocks -------------------------------------------------------

    fn emit_if_block(&mut self, b: &IfBlock<'_>, is_else_if: bool) {
        if !is_else_if {
            self.write("{#if ");
        } else {
            self.write("{:else if ");
        }
        self.write(&print_expression_str(&b.test));
        self.write("}");
        self.emit_block_body(&b.consequent);

        if let Some(alt) = &b.alternate {
            // Detect `{:else if}` chain: alternate is a single IfBlock.
            let alt_nodes = clean_and_sequence(&alt.nodes);
            if alt_nodes.len() == 1 && alt_nodes[0].len() == 1 {
                if let SeqItem::Node(FragmentChild::IfBlock(else_if)) = &alt_nodes[0][0] {
                    self.emit_if_block(else_if, true);
                    if !is_else_if {
                        self.write("{/if}");
                    }
                    return;
                }
            }
            self.write("{:else}");
            self.emit_block_body(alt);
        }

        if !is_else_if {
            self.write("{/if}");
        }
    }

    fn emit_each_block(&mut self, b: &EachBlock) {
        self.write("{#each ");
        self.write(&print_expression_str(&b.expression));
        if let Some(ctx) = &b.context {
            self.write(" as ");
            self.write(&print_pattern_str(ctx));
        }
        if let Some(idx) = &b.index {
            self.write(", ");
            self.write(idx);
        }
        if let Some(key) = &b.key {
            self.write(" (");
            self.write(&print_expression_str(key));
            self.write(")");
        }
        self.write("}");
        // Empty body — upstream emits `...` placeholder.
        let trimmed = trim_boundary_ws(&b.body.nodes);
        if trimmed.is_empty() {
            self.indent_in();
            self.newline();
            self.write("...");
            self.indent_out();
            self.newline();
        } else {
            self.emit_block_body(&b.body);
        }
        if let Some(fallback) = &b.fallback {
            self.write("{:else}");
            self.emit_block_body(fallback);
        }
        self.write("{/each}");
    }

    fn emit_await_block(&mut self, b: &AwaitBlock) {
        self.write("{#await ");
        self.write(&print_expression_str(&b.expression));
        if let Some(pending) = &b.pending {
            self.write("}");
            self.emit_block_body(pending);
            if let Some(then) = &b.then {
                self.write("{:then");
                if let Some(v) = &b.value {
                    self.write(" ");
                    self.write(&print_pattern_str(v));
                }
                self.write("}");
                self.emit_block_body(then);
            }
        } else if let Some(then) = &b.then {
            self.write(" then");
            if let Some(v) = &b.value {
                self.write(" ");
                self.write(&print_pattern_str(v));
            }
            self.write("}");
            self.emit_block_body(then);
        } else {
            self.write("}");
        }
        if let Some(catch) = &b.catch_ {
            self.write("{:catch");
            if let Some(e) = &b.error {
                self.write(" ");
                self.write(&print_pattern_str(e));
            }
            self.write("}");
            self.emit_block_body(catch);
        }
        self.write("{/await}");
    }

    fn emit_key_block(&mut self, b: &KeyBlock) {
        self.write("{#key ");
        self.write(&print_expression_str(&b.expression));
        self.write("}");
        self.emit_block_body(&b.fragment);
        self.write("{/key}");
    }

    fn emit_snippet_block(&mut self, b: &SnippetBlock) {
        self.write("{#snippet ");
        self.write(&b.expression.name);
        self.write("(");
        for (i, p) in b.parameters.iter().enumerate() {
            if i > 0 {
                self.write(", ");
            }
            self.write(&print_pattern_str(p));
        }
        self.write(")}");
        self.emit_block_body(&b.body);
        self.write("{/snippet}");
    }
}

// ---- helpers ------------------------------------------------------------

enum SeqItem<'a> {
    Node(&'a FragmentChild<'a>),
    Text(String),
}

/// Split a fragment's children into "sequences" — runs of inline-shaped
/// nodes that should render on one line. Block-shaped nodes flush both
/// before and after, so each block sits on its own line.
///
/// Mirrors upstream's Fragment visitor (print/index.js:367-474).
fn clean_and_sequence<'a>(nodes: &'a [FragmentChild<'a>]) -> Vec<Vec<SeqItem<'a>>> {
    let mut items: Vec<Vec<SeqItem<'a>>> = Vec::new();
    let mut seq: Vec<SeqItem<'a>> = Vec::new();
    let flush = |items: &mut Vec<Vec<SeqItem<'a>>>, seq: &mut Vec<SeqItem<'a>>| {
        if !seq.is_empty() {
            items.push(std::mem::take(seq));
        }
    };
    for (i, node) in nodes.iter().enumerate() {
        let prev = if i > 0 { Some(&nodes[i - 1]) } else { None };
        let next = if i + 1 < nodes.len() { Some(&nodes[i + 1]) } else { None };
        match node {
            FragmentChild::Text(t) => {
                // Replace whitespace-runs with single space.
                let mut data: String = collapse_internal_ws(t.data.as_str());
                if i == 0 {
                    data = data.trim_start().to_string();
                }
                if i == nodes.len() - 1 {
                    data = data.trim_end().to_string();
                }
                if data.is_empty() {
                    continue;
                }
                if data.starts_with(' ')
                    && prev.is_some_and(|p| !matches!(p, FragmentChild::ExpressionTag(_)))
                {
                    flush(&mut items, &mut seq);
                    data = data.trim_start().to_string();
                }
                if !data.is_empty() {
                    seq.push(SeqItem::Text(data.clone()));
                    if data.ends_with(' ')
                        && next.is_some_and(|n| !matches!(n, FragmentChild::ExpressionTag(_)))
                    {
                        flush(&mut items, &mut seq);
                    }
                }
            }
            _ => {
                if is_block_element(node) {
                    flush(&mut items, &mut seq);
                    seq.push(SeqItem::Node(node));
                    flush(&mut items, &mut seq);
                } else {
                    seq.push(SeqItem::Node(node));
                }
            }
        }
    }
    flush(&mut items, &mut seq);
    items
}

fn is_block_element(n: &FragmentChild<'_>) -> bool {
    matches!(
        n,
        FragmentChild::RegularElement(_)
            | FragmentChild::Component(_)
            | FragmentChild::SlotElement(_)
            | FragmentChild::TitleElement(_)
            | FragmentChild::SvelteElement(_)
            | FragmentChild::SvelteComponent(_)
            | FragmentChild::SvelteSelf(_)
            | FragmentChild::SvelteFragment(_)
            | FragmentChild::SvelteBoundary(_)
            | FragmentChild::SvelteBody(_)
            | FragmentChild::SvelteHead(_)
            | FragmentChild::SvelteDocument(_)
            | FragmentChild::SvelteWindow(_)
            | FragmentChild::IfBlock(_)
            | FragmentChild::EachBlock(_)
            | FragmentChild::AwaitBlock(_)
            | FragmentChild::KeyBlock(_)
            | FragmentChild::SnippetBlock(_)
    )
}

fn collapse_internal_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}

fn measure_width(s: &str) -> usize {
    s.lines().map(|l| l.chars().count()).max().unwrap_or(0)
}

fn trim_boundary_ws<'a>(nodes: &'a [FragmentChild<'a>]) -> &'a [FragmentChild<'a>] {
    let is_ws =
        |n: &FragmentChild<'_>| matches!(n, FragmentChild::Text(t) if t.data.trim().is_empty());
    let mut start = 0;
    let mut end = nodes.len();
    while start < end && is_ws(&nodes[start]) {
        start += 1;
    }
    while end > start && is_ws(&nodes[end - 1]) {
        end -= 1;
    }
    &nodes[start..end]
}

// ---- attribute serialization ------------------------------------------

fn attribute_str(a: &Attribute<'_>) -> String {
    match &a.value {
        AttributeValue::Empty => a.name.to_string(),
        AttributeValue::Single(tag) => {
            // Shorthand: `<X {name}>` when expression is `Identifier(name)`.
            if let svelte_js_ast::Expression::Identifier(id) = &tag.expression {
                if id.name == a.name {
                    // Upstream's print expands `{name}` → `name={name}` for
                    // consistency. Mirror that. (See print/index.js for
                    // `attribute` visitor — it always writes both halves.)
                    return format!("{}={{{}}}", a.name, id.name);
                }
            }
            format!("{}={{{}}}", a.name, print_expression_str(&tag.expression))
        }
        AttributeValue::Many(parts) => {
            if parts.len() == 1 {
                match &parts[0] {
                    AttributeValuePart::Text(t) => {
                        format!("{}=\"{}\"", a.name, t.raw)
                    }
                    AttributeValuePart::ExpressionTag(tag) => {
                        format!("{}={{{}}}", a.name, print_expression_str(&tag.expression))
                    }
                }
            } else {
                // Concatenated parts → quoted template.
                let mut s = String::new();
                s.push_str(&a.name);
                s.push_str("=\"");
                for p in parts {
                    match p {
                        AttributeValuePart::Text(t) => s.push_str(&t.raw),
                        AttributeValuePart::ExpressionTag(tag) => {
                            s.push('{');
                            s.push_str(&print_expression_str(&tag.expression));
                            s.push('}');
                        }
                    }
                }
                s.push('"');
                s
            }
        }
    }
}

fn spread_attribute_str(s: &SpreadAttribute) -> String {
    format!("{{...{}}}", print_expression_str(&s.expression))
}

fn animate_str(d: &AnimateDirective) -> String {
    let mut s = format!("animate:{}", d.name);
    for m in &d.modifiers {
        s.push('|');
        s.push_str(m);
    }
    if let Some(e) = &d.expression {
        s.push_str("={");
        s.push_str(&print_expression_str(e));
        s.push('}');
    }
    s
}

fn bind_str(d: &BindDirective) -> String {
    let mut s = format!("bind:{}", d.name);
    for m in &d.modifiers {
        s.push('|');
        s.push_str(m);
    }
    // Shorthand: `bind:value` when expression is `Identifier(value)`.
    if let svelte_js_ast::Expression::Identifier(id) = &d.expression {
        if id.name == d.name {
            return s;
        }
    }
    s.push_str("={");
    s.push_str(&print_expression_str(&d.expression));
    s.push('}');
    s
}

fn class_str(d: &ClassDirective) -> String {
    let mut s = format!("class:{}", d.name);
    for m in &d.modifiers {
        s.push('|');
        s.push_str(m);
    }
    // Shorthand: `class:active` when expression is `Identifier(active)`.
    if let svelte_js_ast::Expression::Identifier(id) = &d.expression {
        if id.name == d.name {
            return s;
        }
    }
    s.push_str("={");
    s.push_str(&print_expression_str(&d.expression));
    s.push('}');
    s
}

fn let_str(d: &LetDirective) -> String {
    let mut s = format!("let:{}", d.name);
    for m in &d.modifiers {
        s.push('|');
        s.push_str(m);
    }
    if let Some(e) = &d.expression {
        s.push_str("={");
        s.push_str(&print_expression_str(e));
        s.push('}');
    }
    s
}

fn on_str(d: &OnDirective) -> String {
    let mut s = format!("on:{}", d.name);
    for m in &d.modifiers {
        s.push('|');
        s.push_str(m);
    }
    if let Some(e) = &d.expression {
        s.push_str("={");
        s.push_str(&print_expression_str(e));
        s.push('}');
    }
    s
}

fn style_directive_str(d: &StyleDirective) -> String {
    let mut s = format!("style:{}", d.name);
    for m in &d.modifiers {
        s.push('|');
        s.push_str(m);
    }
    match &d.value {
        AttributeValue::Empty => s,
        AttributeValue::Single(tag) => {
            s.push_str("={");
            s.push_str(&print_expression_str(&tag.expression));
            s.push('}');
            s
        }
        AttributeValue::Many(parts) => {
            if parts.len() == 1 {
                match &parts[0] {
                    AttributeValuePart::Text(t) => {
                        s.push_str("=\"");
                        s.push_str(&t.raw);
                        s.push('"');
                    }
                    AttributeValuePart::ExpressionTag(tag) => {
                        s.push_str("={");
                        s.push_str(&print_expression_str(&tag.expression));
                        s.push('}');
                    }
                }
                return s;
            }
            s.push_str("=\"");
            for p in parts {
                match p {
                    AttributeValuePart::Text(t) => s.push_str(&t.raw),
                    AttributeValuePart::ExpressionTag(tag) => {
                        s.push('{');
                        s.push_str(&print_expression_str(&tag.expression));
                        s.push('}');
                    }
                }
            }
            s.push('"');
            s
        }
    }
}

fn transition_str(d: &TransitionDirective) -> String {
    let prefix = match (d.intro, d.outro) {
        (true, false) => "in",
        (false, true) => "out",
        _ => "transition",
    };
    let mut s = format!("{}:{}", prefix, d.name);
    for m in &d.modifiers {
        s.push('|');
        s.push_str(m);
    }
    if let Some(e) = &d.expression {
        s.push_str("={");
        s.push_str(&print_expression_str(e));
        s.push('}');
    }
    s
}

fn use_str(d: &UseDirective) -> String {
    let mut s = format!("use:{}", d.name);
    for m in &d.modifiers {
        s.push('|');
        s.push_str(m);
    }
    if let Some(e) = &d.expression {
        s.push_str("={");
        s.push_str(&print_expression_str(e));
        s.push('}');
    }
    s
}

fn attach_str(t: &AttachTag) -> String {
    format!("{{@attach {}}}", print_expression_str(&t.expression))
}
