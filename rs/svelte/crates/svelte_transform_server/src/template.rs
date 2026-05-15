//! Server-side template lowering.
//!
//! Walks a Svelte `Fragment` and produces a sequence of [`TemplateOp`]s — runs
//! of `$$renderer.push(\`...\`)` template-literal chunks interleaved with raw
//! JS statements (control flow lowered from `{#if}` / `{#each}` / etc.).
//!
//! Mirrors `phases/3-transform/server/visitors/{Fragment, RegularElement,
//! IfBlock, EachBlock, ExpressionTag, HtmlTag, ...}.js`.

use serde_json::Value;

use svelte_ast::attributes::{Attribute, AttributeValue, AttributeValuePart, ElementAttribute};
use svelte_ast::blocks::{AwaitBlock, EachBlock, IfBlock, KeyBlock, SnippetBlock};
use svelte_ast::elements::{Component, RegularElement};
use svelte_ast::fragment::{Fragment, FragmentChild};
use svelte_transform_shared::builders as b;

/// One operation in a lowered fragment. `Push` is a contiguous HTML segment
/// (possibly with embedded `${...}` interpolations); `Stmt` is a JS statement
/// emitted between segments to model `{#if}`/`{#each}` lowering.
#[derive(Debug)]
pub enum TemplateOp {
    Push(TemplateChunks),
    Stmt(Value),
}

/// Alternating raw HTML chunks and dynamic expressions.
/// `quasis.len() == expressions.len() + 1`.
#[derive(Default, Debug)]
pub struct TemplateChunks {
    pub quasis: Vec<String>,
    pub expressions: Vec<Value>,
}

impl TemplateChunks {
    fn new() -> Self {
        Self {
            quasis: vec![String::new()],
            expressions: Vec::new(),
        }
    }
    fn push_str(&mut self, s: &str) {
        self.quasis
            .last_mut()
            .expect("invariant: quasis always non-empty")
            .push_str(s);
    }
    fn push_char(&mut self, c: char) {
        self.quasis
            .last_mut()
            .expect("invariant: quasis always non-empty")
            .push(c);
    }
    fn push_expression(&mut self, expr: Value) {
        self.expressions.push(expr);
        self.quasis.push(String::new());
    }
    fn is_empty(&self) -> bool {
        self.expressions.is_empty() && self.quasis.iter().all(|q| q.is_empty())
    }
}

/// Top-level entry: lower a fragment to a list of ops. Pure-HTML fragments
/// produce a single `Push(...)`. Fragments containing blocks produce a mix
/// of `Push`/`Stmt` segments.
pub fn lower_fragment(fragment: &Fragment) -> Vec<TemplateOp> {
    let mut acc = Accumulator::new();
    for child in &fragment.nodes {
        lower_child(child, &mut acc);
    }
    acc.into_ops()
}

/// Lower a fragment's nodes but trim leading/trailing whitespace-only Text
/// children. Used at the top level — `</script>\n\n<Component />` shouldn't
/// emit a `$$renderer.push(\`\\n\\n\`)` for the inter-tag whitespace.
pub fn lower_fragment_trimmed(fragment: &Fragment) -> Vec<TemplateOp> {
    let trimmed = trim_fragment_edges(fragment);
    let mut acc = Accumulator::new();
    for child in &trimmed {
        lower_child(child, &mut acc);
    }
    acc.into_ops()
}

/// Lower a fragment's nodes but trim leading/trailing whitespace-only Text
/// children, and prepend `<!---->` if the body contains any dynamic content.
/// Used for each/if/key/await block bodies — see upstream `clean_nodes` /
/// the per-iteration anchor-comment behaviour in `Fragment.js`.
pub fn lower_fragment_with_marker(fragment: &Fragment) -> Vec<TemplateOp> {
    let trimmed = trim_fragment_edges(fragment);
    let has_dynamic = trimmed.iter().any(|n| matches!(
        n,
        FragmentChild::ExpressionTag(_)
            | FragmentChild::HtmlTag(_)
            | FragmentChild::IfBlock(_)
            | FragmentChild::EachBlock(_)
            | FragmentChild::AwaitBlock(_)
            | FragmentChild::KeyBlock(_)
            | FragmentChild::Component(_)
            | FragmentChild::RenderTag(_)
    ));
    let mut acc = Accumulator::new();
    if has_dynamic {
        acc.push_str("<!---->");
    }
    for child in &trimmed {
        lower_child(child, &mut acc);
    }
    acc.into_ops()
}

struct Accumulator {
    ops: Vec<TemplateOp>,
    current: TemplateChunks,
}

impl Accumulator {
    fn new() -> Self {
        Self {
            ops: Vec::new(),
            current: TemplateChunks::new(),
        }
    }
    fn push_str(&mut self, s: &str) {
        self.current.push_str(s);
    }
    fn push_char(&mut self, c: char) {
        self.current.push_char(c);
    }
    fn push_expression(&mut self, e: Value) {
        self.current.push_expression(e);
    }
    fn flush(&mut self) {
        if !self.current.is_empty() {
            self.ops
                .push(TemplateOp::Push(std::mem::replace(&mut self.current, TemplateChunks::new())));
        }
    }
    fn stmt(&mut self, s: Value) {
        self.flush();
        self.ops.push(TemplateOp::Stmt(s));
    }
    fn into_ops(mut self) -> Vec<TemplateOp> {
        self.flush();
        self.ops
    }
}

fn lower_child(child: &FragmentChild, acc: &mut Accumulator) {
    match child {
        FragmentChild::Text(t) => acc.push_str(&collapse_whitespace(&t.data)),
        FragmentChild::RegularElement(el) => lower_regular_element(el, acc),
        FragmentChild::ExpressionTag(tag) => {
            // Constant folding: `{'literal'}` becomes the literal characters
            // directly in the template. Mirrors upstream's `ExpressionTag.js`
            // when the expression is a static Literal string.
            if let Some(s) = as_string_literal(&tag.expression) {
                acc.push_str(&s);
            } else {
                let escaped = b::call(
                    b::member(b::id("$"), b::id("escape"), false, false),
                    vec![tag.expression.clone()],
                );
                acc.push_expression(escaped);
            }
        }
        FragmentChild::HtmlTag(tag) => {
            // `{@html expr}` → `${$.html(expr)}` server-side. The runtime
            // helper coerces non-string values and bypasses escape.
            let call = b::call(
                b::member(b::id("$"), b::id("html"), false, false),
                vec![tag.expression.clone()],
            );
            acc.push_expression(call);
        }
        FragmentChild::ConstTag(tag) => {
            // `{@const x = expr}` — emit the variable declaration as a JS stmt.
            acc.stmt(tag.declaration.clone());
        }
        FragmentChild::DebugTag(_) => {
            // `{@debug ...}` — server emits nothing (debugger statements are
            // dev-only and SSR cannot use them). Upstream's DebugTag.js
            // matches this no-op behavior outside `dev` builds.
        }
        FragmentChild::RenderTag(tag) => {
            // `{@render snippet(args)}` lowers to an immediate call of the
            // snippet identifier with `$$renderer` prepended.
            // The expression is typically `name(arg1, arg2)`.
            // For server: emit `snippet($$renderer, arg1, arg2);` as a statement.
            let injected = inject_renderer_into_call(&tag.expression);
            acc.stmt(b::stmt(injected));
        }
        FragmentChild::IfBlock(blk) => lower_if_block(blk, acc),
        FragmentChild::EachBlock(blk) => lower_each_block(blk, acc),
        FragmentChild::Component(c) => lower_component(c, acc),
        FragmentChild::KeyBlock(blk) => lower_key_block(blk, acc),
        FragmentChild::AwaitBlock(blk) => lower_await_block(blk, acc),
        FragmentChild::SnippetBlock(blk) => lower_snippet_block(blk, acc),
        FragmentChild::SvelteHead(el) => lower_svelte_head(el, acc),
        FragmentChild::SvelteBody(el) => lower_svelte_body(el, acc),
        FragmentChild::SvelteFragment(el) => lower_svelte_fragment(el, acc),
        FragmentChild::TitleElement(el) => lower_title_element(el, acc),
        FragmentChild::Comment(_) => {
            // HTML comments are stripped server-side by default.
        }
        // Components, snippets, key/await blocks, svelte:*, etc. — TODO.
        _ => {}
    }
}

fn lower_regular_element(el: &RegularElement, acc: &mut Accumulator) {
    acc.push_char('<');
    acc.push_str(&el.name);
    for attr in &el.attributes {
        lower_attribute(attr, acc);
    }
    if is_void_element(&el.name) {
        acc.push_char('>');
        return;
    }
    acc.push_char('>');
    // Trim leading/trailing whitespace inside the element so `<div>\n\t<p>...</p>\n</div>`
    // becomes `<div><p>...</p></div>`. Mirrors upstream's `clean_nodes` collapse pass.
    let trimmed = trim_fragment_edges(&el.fragment);
    for child in &trimmed {
        lower_child(child, acc);
    }
    acc.push_str("</");
    acc.push_str(&el.name);
    acc.push_char('>');
}

fn lower_attribute(attr: &ElementAttribute, acc: &mut Accumulator) {
    let ElementAttribute::Attribute(Attribute { name, value, .. }) = attr else {
        // Directives (bind:, use:, transition:, etc.) are stripped server-side.
        return;
    };
    // Event-handler attributes (`onclick`, `onkeydown`, …) are dropped on the
    // server — there's no DOM to attach them to.
    if is_event_attribute(name) {
        return;
    }
    match value {
        AttributeValue::Empty(true) => {
            acc.push_char(' ');
            acc.push_str(name);
        }
        AttributeValue::Empty(false) => {}
        AttributeValue::Many(parts) => {
            let all_text = parts
                .iter()
                .all(|p| matches!(p, AttributeValuePart::Text(_)));
            acc.push_char(' ');
            acc.push_str(name);
            acc.push_str("=\"");
            if all_text {
                let mut text = String::new();
                for p in parts {
                    if let AttributeValuePart::Text(t) = p {
                        text.push_str(&t.data);
                    }
                }
                acc.push_str(&escape_attribute_value(&text));
            } else {
                for p in parts {
                    match p {
                        AttributeValuePart::Text(t) => acc.push_str(&t.data),
                        AttributeValuePart::ExpressionTag(tag) => {
                            let escaped = b::call(
                                b::member(b::id("$"), b::id("escape"), false, false),
                                vec![tag.expression.clone()],
                            );
                            acc.push_expression(escaped);
                        }
                    }
                }
            }
            acc.push_char('"');
        }
        AttributeValue::Single(tag) => {
            // Dynamic single-value attribute → `${$.attr('name', value)}` which
            // handles undefined/null skipping at runtime. Matches upstream
            // `AttributeUtils.js`'s `serialize_attribute_value` server path.
            let attr_call = b::call(
                b::member(b::id("$"), b::id("attr"), false, false),
                vec![b::literal_str(name), tag.expression.clone()],
            );
            acc.push_expression(attr_call);
        }
    }
}

fn lower_svelte_head(el: &svelte_ast::SvelteHead, acc: &mut Accumulator) {
    // `<svelte:head>...</svelte:head>` lowers to `$$renderer.head($$renderer => {
    // ...body... })`. Anything inside gets emitted via a nested $$renderer.
    let body = ops_to_statements(lower_fragment_trimmed(&el.fragment));
    let call = b::call(
        b::member(b::id("$$renderer"), b::id("head"), false, false),
        vec![b::arrow(vec![b::id("$$renderer")], b::block(body), false)],
    );
    acc.stmt(b::stmt(call));
}

fn lower_svelte_body(el: &svelte_ast::SvelteBody, acc: &mut Accumulator) {
    // `<svelte:body>` is a no-op for SSR (no DOM to attach to). Attributes
    // (like `bind:visibilityState`) and event handlers can be ignored.
    let _ = el;
    let _ = acc;
}

fn lower_svelte_fragment(el: &svelte_ast::SvelteFragment, acc: &mut Accumulator) {
    // `<svelte:fragment slot="...">...</svelte:fragment>` — for server, just
    // emit the body. The slot fill is handled at component-call time.
    for child in &el.fragment.nodes {
        lower_child(child, acc);
    }
}

fn lower_title_element(el: &svelte_ast::TitleElement, acc: &mut Accumulator) {
    // `<title>...</title>` produces `$$renderer.title(\`<title>{content}</title>\`)`.
    let trimmed = trim_fragment_edges(&el.fragment);
    let mut inner_acc = Accumulator::new();
    inner_acc.push_str("<title>");
    for child in &trimmed {
        lower_child(child, &mut inner_acc);
    }
    inner_acc.push_str("</title>");
    let ops = inner_acc.into_ops();
    let stmts = ops_to_statements(ops);
    // wrap the push() in a $$renderer.title() invocation
    let title_call = b::call(
        b::member(b::id("$$renderer"), b::id("title"), false, false),
        vec![b::arrow(vec![b::id("$$renderer")], b::block(stmts), false)],
    );
    acc.stmt(b::stmt(title_call));
}

fn lower_component(c: &Component, acc: &mut Accumulator) {
    // Server `<Foo prop={x} />` lowering (simplified from `Component.js`):
    //   Foo($$renderer, { prop: x })
    // Directives (bind:, on:, etc.) are dropped server-side. Spread (`{...obj}`)
    // becomes a SpreadElement in the props object. Children content (the
    // component's slot fragment) gets wrapped as a `children: ($$renderer) =>
    // { ... }` callback plus a `$$slots: { default: true }` flag.
    let mut props: Vec<Value> = Vec::new();
    for attr in &c.attributes {
        match attr {
            ElementAttribute::Attribute(Attribute { name, value, .. }) => {
                let value_expr = match value {
                    AttributeValue::Empty(_) => b::literal_bool(true),
                    AttributeValue::Single(tag) => tag.expression.clone(),
                    AttributeValue::Many(parts) => {
                        // Build a string concatenation / template literal for mixed parts.
                        // For simplicity: if all are Text, emit a string literal.
                        let mut s = String::new();
                        let mut all_text = true;
                        for p in parts {
                            match p {
                                AttributeValuePart::Text(t) => s.push_str(&t.data),
                                AttributeValuePart::ExpressionTag(_) => {
                                    all_text = false;
                                    break;
                                }
                            }
                        }
                        if all_text {
                            b::literal_str(&s)
                        } else {
                            // template literal `s${e}s${e}...`
                            let mut quasis: Vec<String> = vec![String::new()];
                            let mut exprs: Vec<Value> = Vec::new();
                            for p in parts {
                                match p {
                                    AttributeValuePart::Text(t) => {
                                        quasis.last_mut().unwrap().push_str(&t.data);
                                    }
                                    AttributeValuePart::ExpressionTag(tag) => {
                                        exprs.push(tag.expression.clone());
                                        quasis.push(String::new());
                                    }
                                }
                            }
                            let qrefs: Vec<&str> =
                                quasis.iter().map(|s| s.as_str()).collect();
                            b::template_literal(qrefs, exprs)
                        }
                    }
                };
                props.push(b::init(name, value_expr));
            }
            ElementAttribute::SpreadAttribute(s) => {
                props.push(b::spread(s.expression.clone()));
            }
            // Directives are stripped server-side.
            _ => {}
        }
    }

    // If the component has children, add `children: ($$renderer) => { ... }` and
    // `$$slots: { default: true }`. Matches upstream `Component.js`.
    let has_children = !c.fragment.nodes.is_empty()
        && c.fragment
            .nodes
            .iter()
            .any(|n| !matches!(n, FragmentChild::Text(t) if t.data.trim().is_empty()));
    if has_children {
        let children_body = ops_to_statements(lower_fragment_with_marker(&c.fragment));
        props.push(b::init(
            "children",
            b::arrow(vec![b::id("$$renderer")], b::block(children_body), false),
        ));
        props.push(b::init(
            "$$slots",
            b::object(vec![b::init("default", b::literal_bool(true))]),
        ));
    }

    acc.stmt(b::stmt(b::call(
        b::id(&c.name),
        vec![b::id("$$renderer"), b::object(props)],
    )));
}

fn lower_if_block(blk: &IfBlock, acc: &mut Accumulator) {
    // Server-side `{#if}` lowering (matches `IfBlock.js`):
    //   $$renderer.push(`<!--[-->`)
    //   if (test) { <consequent> } else { <alternate> }
    //   $$renderer.push(`<!--]-->`)
    // Each branch is its own nested set of $$renderer.push(...) calls.
    acc.push_str("<!--[-->");
    let consequent_body = ops_to_statements(lower_fragment_with_marker(&blk.consequent));
    let alternate_body = blk
        .alternate
        .as_ref()
        .map(|f| ops_to_statements(lower_fragment_with_marker(f)));

    let if_stmt = b::if_stmt(
        blk.test.clone(),
        b::block(consequent_body),
        alternate_body.map(|stmts| b::block(stmts)),
    );
    acc.stmt(if_stmt);
    acc.push_str("<!--]-->");
}

/// Trim leading/trailing whitespace-only Text nodes from a fragment, and
/// strip leading whitespace from the first remaining text + trailing
/// whitespace from the last remaining text. Mirrors `clean_nodes` from
/// upstream `utils.js`.
fn trim_fragment_edges(fragment: &Fragment) -> Vec<FragmentChild> {
    let mut nodes: Vec<FragmentChild> = fragment.nodes.clone();
    while nodes
        .first()
        .map(|n| matches!(n, FragmentChild::Text(t) if t.data.trim().is_empty()))
        .unwrap_or(false)
    {
        nodes.remove(0);
    }
    while nodes
        .last()
        .map(|n| matches!(n, FragmentChild::Text(t) if t.data.trim().is_empty()))
        .unwrap_or(false)
    {
        nodes.pop();
    }
    // Strip leading whitespace from the first text node (if any), trailing
    // whitespace from the last text node (if any). This handles e.g.
    // `\n\tclicks: {count}\n` → `clicks: {count}`.
    if let Some(FragmentChild::Text(t)) = nodes.first_mut() {
        t.data = t.data.trim_start().to_string();
    }
    if let Some(FragmentChild::Text(t)) = nodes.last_mut() {
        t.data = t.data.trim_end().to_string();
    }
    nodes
}

fn lower_each_block(blk: &EachBlock, acc: &mut Accumulator) {
    // Server `{#each}` lowering (simplified — see `EachBlock.js` for full).
    //   $$renderer.push(`<!--[-->`)
    //   const each_array = $.ensure_array_like(expression);
    //   for (let $$index = 0, $$length = each_array.length; $$index < $$length; $$index++) {
    //     let CONTEXT = each_array[$$index];
    //     <body>
    //   }
    //   $$renderer.push(`<!--]-->`)
    acc.push_str("<!--[-->");

    let array_decl = b::const_decl(
        "each_array",
        b::call(
            b::member(b::id("$"), b::id("ensure_array_like"), false, false),
            vec![blk.expression.clone()],
        ),
    );

    // Pick the loop-counter name: if `context` is None and `index` is Some,
    // upstream uses the index name directly as the for-loop counter so the
    // body sees a normal variable rather than a renamed `$$index`. Otherwise
    // we use `$$index` and declare both `let CONTEXT = each_array[$$index]`
    // and `const INDEX = $$index;`.
    let counter_name: String = match (&blk.context, &blk.index) {
        (None, Some(i)) => i.clone(),
        _ => "$$index".to_string(),
    };

    let init = serde_json::json!({
        "type": "VariableDeclaration",
        "kind": "let",
        "declarations": [
            { "type": "VariableDeclarator", "id": b::id(&counter_name), "init": b::literal_num(0.0) },
            {
                "type": "VariableDeclarator",
                "id": b::id("$$length"),
                "init": b::member(b::id("each_array"), b::id("length"), false, false)
            }
        ]
    });
    let test = b::binary("<", b::id(&counter_name), b::id("$$length"));
    let update = b::update("++", b::id(&counter_name), false);

    let mut body_stmts: Vec<Value> = Vec::new();
    if let Some(ctx) = &blk.context {
        body_stmts.push(b::declaration(
            "let",
            vec![b::declarator(
                ctx.clone(),
                Some(b::member(b::id("each_array"), b::id(&counter_name), true, false)),
            )],
        ));
        if let Some(idx) = &blk.index {
            // When both CONTEXT and INDEX present: counter is `$$index`,
            // index variable is a separate `const idx = $$index;`.
            body_stmts.push(b::const_decl(idx, b::id(&counter_name)));
        }
    }
    body_stmts.extend(ops_to_statements(lower_fragment_with_marker(&blk.body)));

    let for_stmt = serde_json::json!({
        "type": "ForStatement",
        "init": init,
        "test": test,
        "update": update,
        "body": b::block(body_stmts)
    });

    acc.stmt(array_decl);
    acc.stmt(for_stmt);
    acc.push_str("<!--]-->");
}

fn lower_key_block(blk: &KeyBlock, acc: &mut Accumulator) {
    // `{#key expr}body{/key}` — server-side, the key is irrelevant (no DOM to
    // diff). Upstream emits the body wrapped in the same marker comments as if-blocks.
    acc.push_str("<!--[-->");
    for child in &blk.fragment.nodes {
        lower_child(child, acc);
    }
    acc.push_str("<!--]-->");
}

fn lower_await_block(blk: &AwaitBlock, acc: &mut Accumulator) {
    // `{#await promise then v}then-body{:catch e}catch-body{/await}` — server emits:
    //   $$renderer.push(`<!--[-->`)
    //   try {
    //     const v = await promise;
    //     <then-body>
    //   } catch (e) {
    //     <catch-body>
    //   }
    //   $$renderer.push(`<!--]-->`)
    // Pending branch is also wrapped in a Promise.race race against a resolved
    // pending, but the simplest approximation just renders the `then` branch.
    acc.push_str("<!--[-->");

    let mut try_body: Vec<Value> = Vec::new();
    if let Some(v) = &blk.value {
        try_body.push(b::declaration(
            "const",
            vec![b::declarator(
                v.clone(),
                Some(serde_json::json!({
                    "type": "AwaitExpression",
                    "argument": blk.expression.clone()
                })),
            )],
        ));
    } else {
        try_body.push(b::stmt(serde_json::json!({
            "type": "AwaitExpression",
            "argument": blk.expression.clone()
        })));
    }
    if let Some(then_frag) = &blk.then {
        try_body.extend(ops_to_statements(lower_fragment(then_frag)));
    }

    let catch_body = blk
        .catch_
        .as_ref()
        .map(|f| ops_to_statements(lower_fragment(f)))
        .unwrap_or_default();

    let try_stmt = serde_json::json!({
        "type": "TryStatement",
        "block": b::block(try_body),
        "handler": {
            "type": "CatchClause",
            "param": blk.error.clone().unwrap_or(b::id("$$error")),
            "body": b::block(catch_body)
        },
        "finalizer": serde_json::Value::Null
    });
    acc.stmt(try_stmt);
    acc.push_str("<!--]-->");
}

fn lower_snippet_block(blk: &SnippetBlock, acc: &mut Accumulator) {
    // `{#snippet name(...params)}body{/snippet}` — server hoists to:
    //   function name(...params) { body }
    // Snippets are first-class JS functions in the output.
    let body = ops_to_statements(lower_fragment(&blk.body));
    let name = blk
        .expression
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("$$snippet");
    acc.stmt(b::function_declaration(
        b::id(name),
        // Server snippet calls take an extra `$$renderer` as first param.
        std::iter::once(b::id("$$renderer"))
            .chain(blk.parameters.iter().cloned())
            .collect(),
        b::block(body),
        false,
    ));
}

/// Convert a `Vec<TemplateOp>` (from `lower_fragment`) into a flat list of
/// `$$renderer.push(...)` / control-flow JS statements suitable as a function
/// body or branch.
pub fn ops_to_statements(ops: Vec<TemplateOp>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for op in ops {
        match op {
            TemplateOp::Push(chunks) => {
                if chunks.is_empty() {
                    continue;
                }
                let quasi_refs: Vec<&str> = chunks.quasis.iter().map(|s| s.as_str()).collect();
                let lit = b::template_literal(quasi_refs, chunks.expressions);
                out.push(b::stmt(b::call(
                    b::member(b::id("$$renderer"), b::id("push"), false, false),
                    vec![lit],
                )));
            }
            TemplateOp::Stmt(s) => out.push(s),
        }
    }
    out
}

/// If `expr` is an acorn `Literal` with a string `value`, return that string.
/// Used for constant-folding `{'foo'}` to inline text in the template.
fn as_string_literal(expr: &Value) -> Option<String> {
    if expr.get("type").and_then(|v| v.as_str()) != Some("Literal") {
        return None;
    }
    expr.get("value")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// `{@render snippet(args)}` — prepend `$$renderer` to the call's argument
/// list so the snippet receives the active renderer. If `expr` isn't a
/// CallExpression, fall back to calling it directly.
fn inject_renderer_into_call(expr: &Value) -> Value {
    if expr.get("type").and_then(|v| v.as_str()) == Some("CallExpression") {
        let mut call = expr.clone();
        if let Some(args) = call.get_mut("arguments").and_then(|v| v.as_array_mut()) {
            args.insert(0, b::id("$$renderer"));
        }
        call
    } else {
        b::call(expr.clone(), vec![b::id("$$renderer")])
    }
}

/// Collapse runs of whitespace in text content to a single space. Matches
/// upstream's default `preserveWhitespace = false` text-node behavior — see
/// `phases/3-transform/server/visitors/shared/utils.js` and
/// `Fragment.js`'s `clean_nodes` text-collapse pass.
fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(ch);
            in_ws = false;
        }
    }
    out
}

fn escape_attribute_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("&quot;"),
            '&' => out.push_str("&amp;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Matches `on*` event handler attribute names that the server should drop.
/// Mirrors upstream's check (`utils/event_attributes.js`).
fn is_event_attribute(name: &str) -> bool {
    if !name.starts_with("on") || name.len() < 3 {
        return false;
    }
    let next = name.as_bytes()[2];
    !next.is_ascii_uppercase() && next != b'-'
}

fn is_void_element(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}
