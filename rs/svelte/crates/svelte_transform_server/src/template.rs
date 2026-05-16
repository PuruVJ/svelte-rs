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
    // If the fragment ends with a Stmt op (Component call, block, ...) AND
    // has earlier push-emitting content, append a `<!---->` anchor marker so
    // the runtime can locate the end of the template. Matches upstream's
    // Fragment.js trailing-marker behavior.
    let mut ops = acc.into_ops();
    let has_push = ops.iter().any(|o| matches!(o, TemplateOp::Push(_)));
    let ends_with_stmt = matches!(ops.last(), Some(TemplateOp::Stmt(_)));
    if has_push && ends_with_stmt {
        let mut marker = TemplateChunks::new();
        marker.push_str("<!---->");
        ops.push(TemplateOp::Push(marker));
    }
    ops
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
    /// Set when last emitted op was a Stmt (Component invocation, block,
    /// etc.). The next push of text/expression content prepends `<!---->`
    /// as an anchor marker so the runtime can locate the end of the
    /// preceding dynamic insertion.
    needs_anchor_marker: bool,
}

impl Accumulator {
    fn new() -> Self {
        Self {
            ops: Vec::new(),
            current: TemplateChunks::new(),
            needs_anchor_marker: false,
        }
    }
    fn maybe_emit_marker(&mut self, next_str_starts_with: Option<&str>) {
        if self.needs_anchor_marker {
            // Suppress the anchor marker when the next content is itself
            // a block marker (e.g. `<!--]-->`). The block-end marker
            // already serves as a position anchor.
            let suppress = next_str_starts_with
                .map(|s| s.starts_with("<!--"))
                .unwrap_or(false);
            if !suppress {
                self.current.push_str("<!---->");
            }
            self.needs_anchor_marker = false;
        }
    }
    fn push_str(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.maybe_emit_marker(Some(s));
        self.current.push_str(s);
    }
    fn push_char(&mut self, c: char) {
        self.maybe_emit_marker(None);
        self.current.push_char(c);
    }
    fn push_expression(&mut self, e: Value) {
        self.maybe_emit_marker(None);
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
        self.needs_anchor_marker = true;
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
            if let Some(s) = as_inlineable_literal(&tag.expression) {
                acc.push_str(&s);
            } else if expression_has_await(&tag.expression) {
                // Async expression tag: emit as
                // `$$renderer.push(async () => $.escape(await EXPR));`
                // — matches upstream's async ExpressionTag handling.
                let escape_call = b::call(
                    b::member(b::id("$"), b::id("escape"), false, false),
                    vec![tag.expression.clone()],
                );
                let async_arrow = serde_json::json!({
                    "type": "ArrowFunctionExpression",
                    "async": true,
                    "generator": false,
                    "params": [],
                    "body": escape_call,
                    "expression": true
                });
                acc.stmt(b::stmt(b::call(
                    b::member(b::id("$$renderer"), b::id("push"), false, false),
                    vec![async_arrow],
                )));
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
        FragmentChild::SvelteElement(el) => lower_svelte_element(el, acc),
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
        // Upstream emits void elements with the self-closing slash. Mirrors
        // `phases/3-transform/server/visitors/RegularElement.js`'s output.
        acc.push_str("/>");
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
    // `bind:value={x}` etc. on form elements lower to a regular dynamic
    // attribute server-side. Other binds (bind:this, bind:innerHTML) drop.
    let (name, value): (&str, &AttributeValue) = match attr {
        ElementAttribute::Attribute(Attribute { name, value, .. }) => (name.as_str(), value),
        ElementAttribute::BindDirective(bd) if is_bindable_value_attribute(&bd.name) => {
            // Emit `${$.attr('value', expr)}` directly for the bind case.
            let attr_call = b::call(
                b::member(b::id("$"), b::id("attr"), false, false),
                vec![b::literal_str(&bd.name), bd.expression.clone()],
            );
            acc.push_expression(attr_call);
            return;
        }
        _ => return,
    };
    if is_event_attribute(name) {
        return;
    }
    match value {
        AttributeValue::Empty(true) => {
            // Upstream's server emits `name=""` (empty-string form), not bare
            // `name`. Matches HTML5 attribute serialization spec.
            acc.push_char(' ');
            acc.push_str(name);
            acc.push_str("=\"\"");
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

fn lower_svelte_element(el: &svelte_ast::SvelteElement, acc: &mut Accumulator) {
    // `<svelte:element this={tag}>...</svelte:element>` → `$.element($$renderer, tag)` for
    // the simplest case. With children, body becomes a callback.
    // Mirrors `phases/3-transform/server/visitors/SvelteElement.js`.
    let has_children = !el.fragment.nodes.is_empty()
        && el
            .fragment
            .nodes
            .iter()
            .any(|n| !matches!(n, FragmentChild::Text(t) if t.data.trim().is_empty()));
    let mut args: Vec<Value> = vec![b::id("$$renderer"), el.tag.clone()];
    if has_children {
        let body = ops_to_statements(lower_fragment_trimmed(&el.fragment));
        args.push(b::arrow(vec![b::id("$$renderer")], b::block(body), false));
    }
    acc.stmt(b::stmt(b::call(
        b::member(b::id("$"), b::id("element"), false, false),
        args,
    )));
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
            // bind:X={target} (NOT bind:this) → getter/setter prop pair.
            // The setter assigns the target and sets \$\$settled = false so
            // the component-body do-while wrapper re-iterates.
            ElementAttribute::BindDirective(bd) if bd.name != "this" => {
                let bind_name = bd.name.clone();
                let target = bd.expression.clone();
                // get NAME() { return target; }
                let getter = serde_json::json!({
                    "type": "Property",
                    "kind": "get",
                    "key": { "type": "Identifier", "name": bind_name.clone() },
                    "value": {
                        "type": "FunctionExpression",
                        "async": false,
                        "generator": false,
                        "id": null,
                        "params": [],
                        "body": {
                            "type": "BlockStatement",
                            "body": [{
                                "type": "ReturnStatement",
                                "argument": target.clone()
                            }]
                        }
                    },
                    "computed": false,
                    "method": false,
                    "shorthand": false
                });
                // set NAME($$value) { target = $$value; $$settled = false; }
                let setter_body = vec![
                    b::stmt(b::assignment("=", target, b::id("$$value"))),
                    b::stmt(b::assignment("=", b::id("$$settled"), b::literal_bool(false))),
                ];
                let setter = serde_json::json!({
                    "type": "Property",
                    "kind": "set",
                    "key": { "type": "Identifier", "name": bind_name },
                    "value": {
                        "type": "FunctionExpression",
                        "async": false,
                        "generator": false,
                        "id": null,
                        "params": [{ "type": "Identifier", "name": "$$value" }],
                        "body": {
                            "type": "BlockStatement",
                            "body": setter_body
                        }
                    },
                    "computed": false,
                    "method": false,
                    "shorthand": false
                });
                props.push(getter);
                props.push(setter);
            }
            // Other directives (on:, use:, etc.) are stripped server-side.
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
    let test_has_await = expression_has_await(&blk.test);
    let body_has_await = fragment_has_top_level_await(&blk.consequent)
        || blk
            .alternate
            .as_ref()
            .map(fragment_has_top_level_await)
            .unwrap_or(false);
    // child_block wrap is only required when the IF TEST itself is async.
    // Body awaits are handled per-element (ExpressionTag etc.). Indexed
    // markers `<!--[0-->` are still used when body has awaits, since the
    // runtime needs them for hydration tracking.
    let wrap_in_child_block = test_has_await;
    let async_mode = test_has_await || body_has_await;

    if async_mode && wrap_in_child_block {
        // Async-mode if-block:
        //   $$renderer.child_block(async ($$renderer) => {
        //     if ((await $.save(test))()) {
        //       $$renderer.push('<!--[0-->');
        //       ...consequent...
        //     } else {
        //       $$renderer.push('<!--[-1-->');
        //       ...alternate...
        //     }
        //   });
        //   $$renderer.push(`<!--]-->`);
        let mut test = blk.test.clone();
        transform_await_to_save_call(&mut test);

        let mut consequent_body = vec![marker_push("<!--[0-->")];
        consequent_body.extend(ops_to_statements(lower_fragment_trimmed(&blk.consequent)));

        let mut alternate_body = vec![marker_push("<!--[-1-->")];
        if let Some(alt) = &blk.alternate {
            alternate_body.extend(ops_to_statements(lower_fragment_trimmed(alt)));
        }

        let if_stmt = b::if_stmt(test, b::block(consequent_body), Some(b::block(alternate_body)));
        let child_block = b::call(
            b::member(b::id("$$renderer"), b::id("child_block"), false, false),
            vec![serde_json::json!({
                "type": "ArrowFunctionExpression",
                "async": true,
                "generator": false,
                "params": [b::id("$$renderer")],
                "body": b::block(vec![if_stmt]),
                "expression": false
            })],
        );
        acc.stmt(b::stmt(child_block));
        acc.push_str("<!--]-->");
        return;
    }

    // Non-(child-block-wrapped) if. Use indexed markers when async_mode (body
    // has awaits), otherwise the simple `<!--[-->`/`<!--]-->` pair.
    if async_mode {
        let mut consequent_body = vec![marker_push("<!--[0-->")];
        consequent_body.extend(ops_to_statements(lower_fragment_trimmed(&blk.consequent)));
        let mut alternate_body = vec![marker_push("<!--[-1-->")];
        if let Some(alt) = &blk.alternate {
            alternate_body.extend(ops_to_statements(lower_fragment_trimmed(alt)));
        }
        let if_stmt = b::if_stmt(
            blk.test.clone(),
            b::block(consequent_body),
            Some(b::block(alternate_body)),
        );
        acc.stmt(if_stmt);
        acc.push_str("<!--]-->");
        return;
    }

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

/// Builds a `$$renderer.push('<!--MARKER-->')` statement for use inside async
/// block bodies. Mirrors upstream's literal-string push at branch entry.
fn marker_push(s: &str) -> Value {
    b::stmt(b::call(
        b::member(b::id("$$renderer"), b::id("push"), false, false),
        vec![b::literal_str(s)],
    ))
}

/// Top-level await detection in a fragment — looks at direct ExpressionTag /
/// HtmlTag / RenderTag / IfBlock test / EachBlock expression / AwaitBlock
/// expression. Does NOT recurse into nested function bodies. Used to decide
/// whether the parent block needs async lowering.
fn fragment_has_top_level_await(f: &Fragment) -> bool {
    for n in &f.nodes {
        match n {
            FragmentChild::ExpressionTag(t) if expression_has_await(&t.expression) => return true,
            FragmentChild::HtmlTag(t) if expression_has_await(&t.expression) => return true,
            FragmentChild::ConstTag(t) if expression_has_await(&t.declaration) => return true,
            FragmentChild::RenderTag(t) if expression_has_await(&t.expression) => return true,
            FragmentChild::IfBlock(b) => {
                if expression_has_await(&b.test)
                    || fragment_has_top_level_await(&b.consequent)
                    || b.alternate
                        .as_ref()
                        .map(fragment_has_top_level_await)
                        .unwrap_or(false)
                {
                    return true;
                }
            }
            FragmentChild::EachBlock(b) => {
                if expression_has_await(&b.expression)
                    || fragment_has_top_level_await(&b.body)
                    || b.fallback
                        .as_ref()
                        .map(fragment_has_top_level_await)
                        .unwrap_or(false)
                {
                    return true;
                }
            }
            FragmentChild::AwaitBlock(_) => return true,
            FragmentChild::KeyBlock(b) => {
                if expression_has_await(&b.expression) || fragment_has_top_level_await(&b.fragment) {
                    return true;
                }
            }
            FragmentChild::RegularElement(el) => {
                if fragment_has_top_level_await(&el.fragment) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Trim leading/trailing whitespace-only Text nodes from a fragment, drop
/// HTML comments (server-side stripped), and strip whitespace adjacent to
/// dropped comments and edge whitespace from the first/last remaining text.
/// Mirrors `clean_nodes` from upstream `utils.js`.
fn trim_fragment_edges(fragment: &Fragment) -> Vec<FragmentChild> {
    let mut nodes: Vec<FragmentChild> = fragment
        .nodes
        .iter()
        .filter(|n| !matches!(n, FragmentChild::Comment(_)))
        .cloned()
        .collect();
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
    if let Some(FragmentChild::Text(t)) = nodes.first_mut() {
        t.data = t.data.trim_start().to_string();
    }
    if let Some(FragmentChild::Text(t)) = nodes.last_mut() {
        t.data = t.data.trim_end().to_string();
    }
    nodes
}

fn lower_each_block(blk: &EachBlock, acc: &mut Accumulator) {
    // Sync vs async lowering: when the each-expression or body contains
    // top-level await, the for-loop body goes inside
    // `$$renderer.child_block(async ($$renderer) => { ... })`.
    let expr_has_await = expression_has_await(&blk.expression);
    let body_has_await = fragment_has_top_level_await(&blk.body);
    let async_mode = expr_has_await || body_has_await;

    acc.push_str("<!--[-->");

    let mut expr = blk.expression.clone();
    if expr_has_await {
        transform_await_to_save_call(&mut expr);
        // The save-call form is `(await $.save(EXPR))()` — wrap accordingly.
    }
    let array_decl = b::const_decl(
        "each_array",
        b::call(
            b::member(b::id("$"), b::id("ensure_array_like"), false, false),
            vec![expr],
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

    let has_fallback = blk.fallback.is_some();

    if async_mode {
        // Async-mode each-block with optional fallback:
        //   $$renderer.child_block(async ($$renderer) => {
        //     const each_array = ...;
        //     if (each_array.length !== 0) {
        //       $$renderer.push('<!--[-->');
        //       for (...) { ... }
        //     } else {
        //       $$renderer.push('<!--[!-->');
        //       <fallback>
        //     }
        //   });
        let mut inner_stmts: Vec<Value> = vec![array_decl];
        if has_fallback {
            let then_body = vec![
                marker_push("<!--[-->"),
                for_stmt,
            ];
            let mut else_body = vec![marker_push("<!--[!-->")];
            else_body.extend(ops_to_statements(lower_fragment_with_marker(
                blk.fallback.as_ref().unwrap(),
            )));
            inner_stmts.push(b::if_stmt(
                b::binary(
                    "!==",
                    b::member(b::id("each_array"), b::id("length"), false, false),
                    b::literal_num(0.0),
                ),
                b::block(then_body),
                Some(b::block(else_body)),
            ));
        } else {
            inner_stmts.push(for_stmt);
        }
        let inner = b::block(inner_stmts);
        let child_block = b::call(
            b::member(b::id("$$renderer"), b::id("child_block"), false, false),
            vec![serde_json::json!({
                "type": "ArrowFunctionExpression",
                "async": true,
                "generator": false,
                "params": [b::id("$$renderer")],
                "body": inner,
                "expression": false
            })],
        );
        // For async-fallback, the `<!--[-->` opener moves inside (above) — strip
        // it from the parent push. The closer remains.
        if has_fallback {
            // Remove the `<!--[-->` we already pushed at the top
            // by replacing the last push op (which was `<!--[-->`).
            // Hack: pop last chunk from current and replace with empty marker.
            if let Some(last) = acc.current.quasis.last_mut() {
                if last.ends_with("<!--[-->") {
                    let new_len = last.len() - "<!--[-->".len();
                    last.truncate(new_len);
                }
            }
        }
        acc.stmt(b::stmt(child_block));
    } else {
        acc.stmt(array_decl);
        if has_fallback {
            // Sync fallback: emit `if (length !== 0) { <!--[--> for-loop } else { <!--[!--> fallback }`
            // Need to undo our top-level `<!--[-->` first.
            if let Some(last) = acc.current.quasis.last_mut() {
                if last.ends_with("<!--[-->") {
                    let new_len = last.len() - "<!--[-->".len();
                    last.truncate(new_len);
                }
            }
            let then_body = vec![marker_push("<!--[-->"), for_stmt];
            let mut else_body = vec![marker_push("<!--[!-->")];
            else_body.extend(ops_to_statements(lower_fragment_trimmed(
                blk.fallback.as_ref().unwrap(),
            )));
            acc.stmt(b::if_stmt(
                b::binary(
                    "!==",
                    b::member(b::id("each_array"), b::id("length"), false, false),
                    b::literal_num(0.0),
                ),
                b::block(then_body),
                Some(b::block(else_body)),
            ));
        } else {
            acc.stmt(for_stmt);
        }
    }
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

/// Walk a body of statements and check if any references `$$renderer`. Used
/// to decide whether the wrapping arrow needs the `$$renderer` parameter.
fn body_uses_renderer(stmts: &[Value]) -> bool {
    fn walk(v: &Value) -> bool {
        match v {
            Value::Array(arr) => arr.iter().any(walk),
            Value::Object(obj) => {
                if obj.get("type").and_then(|v| v.as_str()) == Some("Identifier")
                    && obj.get("name").and_then(|v| v.as_str()) == Some("$$renderer")
                {
                    return true;
                }
                obj.values().any(walk)
            }
            _ => false,
        }
    }
    stmts.iter().any(walk)
}

fn lower_await_block(blk: &AwaitBlock, acc: &mut Accumulator) {
    // Server `{#await promise then v}then-body{:catch e}catch-body{/await}`
    // lowers (non-async mode) to:
    //   $.await($$renderer, promise, pending_cb, then_cb, catch_cb?)
    //   $$renderer.push(`<!--]-->`)
    // The `$.await` runtime emits the `<!--[-->` marker itself.
    // Matches `phases/3-transform/server/visitors/AwaitBlock.js`.
    let pending_body = blk
        .pending
        .as_ref()
        .map(|f| ops_to_statements(lower_fragment_with_marker(f)))
        .unwrap_or_default();
    let pending_uses_renderer = body_uses_renderer(&pending_body);
    let pending_params = if pending_uses_renderer {
        vec![b::id("$$renderer")]
    } else {
        vec![]
    };
    let pending_cb = b::arrow(pending_params, b::block(pending_body), false);

    let then_value = blk
        .value
        .clone()
        .unwrap_or_else(|| b::id("$$value"));
    let then_body = blk
        .then
        .as_ref()
        .map(|f| ops_to_statements(lower_fragment_with_marker(f)))
        .unwrap_or_default();
    let then_uses_renderer = body_uses_renderer(&then_body);
    let then_params = if then_uses_renderer {
        vec![b::id("$$renderer"), then_value]
    } else {
        vec![then_value]
    };
    let then_cb = b::arrow(then_params, b::block(then_body), false);

    let mut args: Vec<Value> = vec![
        b::id("$$renderer"),
        blk.expression.clone(),
        pending_cb,
        then_cb,
    ];
    if blk.catch_.is_some() {
        let catch_param = blk
            .error
            .clone()
            .unwrap_or_else(|| b::id("$$error"));
        let catch_body = blk
            .catch_
            .as_ref()
            .map(|f| ops_to_statements(lower_fragment_with_marker(f)))
            .unwrap_or_default();
        let catch_uses_renderer = body_uses_renderer(&catch_body);
        let catch_params = if catch_uses_renderer {
            vec![b::id("$$renderer"), catch_param]
        } else {
            vec![catch_param]
        };
        args.push(b::arrow(catch_params, b::block(catch_body), false));
    }

    acc.stmt(b::stmt(b::call(
        b::member(b::id("$"), b::id("await"), false, false),
        args,
    )));
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

/// Prepend a `<!---->` marker to the first `Push` op (or insert one if none),
/// then convert to statements. Used for snippet bodies which always require
/// the marker even when content is purely static.
pub fn prepend_marker_and_to_statements(mut ops: Vec<TemplateOp>) -> Vec<Value> {
    let mut prepended = false;
    for op in ops.iter_mut() {
        if let TemplateOp::Push(c) = op {
            if let Some(first) = c.quasis.first_mut() {
                let mut new_s = String::from("<!---->");
                new_s.push_str(first);
                *first = new_s;
            }
            prepended = true;
            break;
        }
    }
    if !prepended {
        let mut marker = TemplateChunks::new();
        marker.push_str("<!---->");
        ops.insert(0, TemplateOp::Push(marker));
    }
    ops_to_statements(ops)
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

/// If `expr` is a constant literal that can be inlined as raw text, return its
/// rendered form. Handles string/number/boolean/null/Identifier-undefined plus
/// nullish-coalescing / logical fold-through (`literal ?? x` → `literal` if
/// non-null; `0 || x` → `x`).
fn as_inlineable_literal(expr: &Value) -> Option<String> {
    let ty = expr.get("type").and_then(|v| v.as_str())?;
    match ty {
        "Literal" => {
            let value = expr.get("value")?;
            match value {
                Value::String(s) => Some(s.clone()),
                Value::Null => Some(String::new()),
                Value::Bool(b) => Some(b.to_string()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            }
        }
        "Identifier" => {
            // `{undefined}` renders as empty on the server.
            if expr.get("name").and_then(|v| v.as_str()) == Some("undefined") {
                Some(String::new())
            } else {
                None
            }
        }
        "CallExpression" => {
            // Constant-fold pure built-ins: `Math.max(0, 1)` → `1`. Only used
            // when ALL arguments are numeric literals — matches upstream's
            // `is_pure` + folding pass in `phases/2-analyze`.
            let callee = expr.get("callee")?;
            let ty = callee.get("type").and_then(|v| v.as_str())?;
            if ty != "MemberExpression" {
                return None;
            }
            let obj = callee.get("object")?.get("name").and_then(|v| v.as_str())?;
            let prop = callee.get("property")?.get("name").and_then(|v| v.as_str())?;
            if obj != "Math" {
                return None;
            }
            let args = expr.get("arguments")?.as_array()?;
            // Recurse into args so `Math.max(0, Math.min(0, 100))` folds to `0`.
            let nums: Vec<f64> = args
                .iter()
                .map(|a| {
                    if a.get("type").and_then(|v| v.as_str()) == Some("Literal") {
                        a.get("value").and_then(|v| v.as_f64())
                    } else {
                        // Try recursive fold — if the inner expression folds
                        // to a number-shaped string, parse it.
                        as_inlineable_literal(a).and_then(|s| s.parse::<f64>().ok())
                    }
                })
                .collect::<Option<Vec<_>>>()?;
            let result = match prop {
                "max" => nums.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
                "min" => nums.iter().cloned().fold(f64::INFINITY, f64::min),
                "abs" if nums.len() == 1 => nums[0].abs(),
                "floor" if nums.len() == 1 => nums[0].floor(),
                "ceil" if nums.len() == 1 => nums[0].ceil(),
                "round" if nums.len() == 1 => nums[0].round(),
                _ => return None,
            };
            if result.is_finite() {
                if result.fract() == 0.0 {
                    Some((result as i64).to_string())
                } else {
                    Some(result.to_string())
                }
            } else {
                None
            }
        }
        "LogicalExpression" => {
            let op = expr.get("operator").and_then(|v| v.as_str())?;
            let left = expr.get("left")?;
            // For `??`: if LHS is a non-null literal, the whole expression is LHS.
            if op == "??" {
                if let Some(s) = as_inlineable_literal(left) {
                    let lv = left.get("value");
                    let is_null = matches!(lv, Some(Value::Null));
                    let is_undef = left.get("type").and_then(|v| v.as_str()) == Some("Identifier")
                        && left.get("name").and_then(|v| v.as_str()) == Some("undefined");
                    if !is_null && !is_undef {
                        return Some(s);
                    }
                    // Otherwise fall through to RHS.
                    return as_inlineable_literal(expr.get("right")?);
                }
            }
            None
        }
        _ => None,
    }
}

/// Returns true if `expr` contains any AwaitExpression node (recursive search).
pub fn expression_has_await(expr: &Value) -> bool {
    fn walk(v: &Value) -> bool {
        match v {
            Value::Array(arr) => arr.iter().any(walk),
            Value::Object(obj) => {
                if obj.get("type").and_then(|v| v.as_str()) == Some("AwaitExpression") {
                    return true;
                }
                // Don't recurse into arrow/function bodies — internal awaits there
                // are part of inner async fns and don't make this expr async.
                let ty = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if matches!(
                    ty,
                    "ArrowFunctionExpression" | "FunctionExpression" | "FunctionDeclaration"
                ) {
                    return false;
                }
                obj.values().any(walk)
            }
            _ => false,
        }
    }
    walk(expr)
}

/// Transform every top-level `await X` in an expression into `(await $.save(X))()`.
/// Used in async-mode `if test` / `each expr` lowering — the save+call pattern
/// caches the awaited value so it can be re-read on subsequent renders.
/// Mirrors upstream's `phases/3-transform/server/visitors/shared/utils.js`
/// transform pass.
pub fn transform_await_to_save_call(expr: &mut Value) {
    fn walk(v: &mut Value) {
        let ty = v
            .get("type")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if matches!(
            ty.as_str(),
            "ArrowFunctionExpression" | "FunctionExpression" | "FunctionDeclaration"
        ) {
            return;
        }
        if ty == "AwaitExpression" {
            let arg = v.get("argument").cloned().unwrap_or(Value::Null);
            // Rewrite: (await $.save(arg))()
            *v = serde_json::json!({
                "type": "CallExpression",
                "callee": {
                    "type": "AwaitExpression",
                    "argument": {
                        "type": "CallExpression",
                        "callee": {
                            "type": "MemberExpression",
                            "object": { "type": "Identifier", "name": "$" },
                            "property": { "type": "Identifier", "name": "save" },
                            "computed": false,
                            "optional": false
                        },
                        "arguments": [arg],
                        "optional": false
                    }
                },
                "arguments": [],
                "optional": false
            });
            return;
        }
        if let Some(arr) = v.as_array_mut() {
            for x in arr {
                walk(x);
            }
        } else if let Some(obj) = v.as_object_mut() {
            for (_, x) in obj.iter_mut() {
                walk(x);
            }
        }
    }
    walk(expr);
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

/// Names of `bind:` directives that should lower to a server-side dynamic
/// attribute (because the value affects the rendered HTML). Other binds
/// (`bind:this`, `bind:innerHTML`, `bind:textContent`, ...) are stripped.
fn is_bindable_value_attribute(name: &str) -> bool {
    matches!(
        name,
        "value" | "checked" | "group" | "files"
    )
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
