# svelte-rs

Rust + WebAssembly port of the Svelte compiler. Lives alongside `packages/svelte/`,
which remains the production JS compiler and the source of truth for behavior.

## Ground rule

The JS sources under `packages/svelte/src/compiler/` are the **only** source of truth.
Behavior, error messages, AST shapes, and sourcemap output are derived from those
files, not from prior knowledge or external docs. The `probe/` directory holds a
Node-side bridge that runs the JS compiler against fixture inputs — the
`svelte_test_harness` binary diffs Rust output against this golden master.

## Layout

```
rs/svelte/
  Cargo.toml             workspace manifest
  rust-toolchain.toml    pinned stable + wasm32 target
  crates/                one crate per pipeline stage / ported npm dep
  probe/run.mjs          Node bridge that runs the JS compiler for diffing
```

## Working with the workspace

```sh
# Build everything (cache deps on first run)
cd rs/svelte && cargo check --workspace

# Run a fixture through the JS compiler via the probe (sanity check plumbing)
cargo run -p svelte_test_harness -- parse packages/svelte/tests/parser-modern/samples/comment-before-script

# Per-crate tests
cargo test -p svelte_ast
```

## JS-compatible API (`@svelte-rs/compiler`)

The Rust compiler exposes the same entry shape as `svelte/compiler`:

```js
import { compile, compileSync } from './pkg/svelte-compiler/index.js';

const result = compileSync(source, {
  generate: 'client',
  filename: 'samples/hello-world/index.svelte',
});
// result.js.code, result.js.map, result.css, result.warnings, result.metadata, result.ast
```

WASM is built with `wasm-pack build crates/svelte_wasm --target nodejs --release`.
The facade normalizes function-valued options (`customElement`, `css`) on the JS
side before calling into Rust. `parse()` still returns `null` until AST
serialization lands.

## Status

**Phase 0** — done. Workspace scaffolded (18 crates), all stubs compile, the
`svelte_test_harness` binary shells out to `probe/run.mjs` which runs the
upstream JS compiler against any fixture under `packages/svelte/tests/`.

**Phase 1** — done.
- `svelte_diagnostics` — 270 diagnostic functions (187 errors + 83 warnings)
  generated at build time from `packages/svelte/messages/**/*.md` (the same
  source the upstream `scripts/process-messages/index.js` consumes). A 13-case
  differential test asserts byte-equal message strings vs. the JS `errors.js`.
- `svelte_ast` — full template AST: Root, Fragment, Text, Comment, Script,
  JsComment, all 6 tag kinds, all 14 element kinds (Component, RegularElement,
  SlotElement, TitleElement, plus svelte:body/boundary/component/document/
  element/fragment/head/options/self/window), all 5 block kinds, all 8
  directive kinds, Attribute, SpreadAttribute. **All 24 parser-modern
  fixtures roundtrip cleanly** through serde — Rust deserializes the live
  JS parser output and re-serializes byte-equivalent JSON.
- `svelte_compiler` facade — `CompileOptions`, `ModuleCompileOptions`,
  `ParseOptions`, including `experimental.async` (Svelte 5.36+). JSON
  shape matches the upstream camelCase contract.
- `svelte_test_harness` — runs both Rust and JS in one shot and prints
  a unified diff. `cargo run -p svelte_test_harness -- all-parser-modern`
  exercises the full parser-modern suite.

**Phase 2** — done.
- 2a/2b: `svelte_parse` scaffold + utility helpers (`is_whitespace`, BOM
  strip, cursor advance, `LineMap` for line/column), text reader, HTML
  comment reader.
- 2c: Element parsing — `RegularElement`, attributes (bare, quoted-string,
  unquoted), nested fragments, void elements, self-closing tags. `name_loc`
  with line/column/character via `LineMap`. `{...}` blocks inside opening
  tags are skipped (placeholder until 2d's real mustache parsing).
- 2d-2g pending: mustache tag parsing (`{expr}`, `{#if}`, etc.) + OXC
  integration for JS expressions + `<script>` + `<style>`.

Run `cargo test --workspace` to see all green (152 tests).
Run `cargo run -q -p svelte_test_harness -- all-parser-modern` and
`-- all-parser-legacy` to see the fixture-suite status. Combined:

- **parser-modern**: **20 match / 0 diverge / 4 error** out of 24.
  All four remaining errors are loose-mode error-recovery fixtures.
- **parser-legacy**: **75 match / 2 diverge / 6 error** out of 83.
  Remaining diverges: `javascript-comments` (trailing-comments on
  expressions) and `unusual-identifier` (UTF-16 encoding mismatch).
  Remaining errors are 5 loose-mode + 1 known JS-side parse error
  (`implicitly-closed-li-block`).
- **Combined: 95 / 107 fixtures (89%)**.

Phase 2 coverage:
- **2d** (mustache `{expr}`, `{@html}`, `{@attach}`, `{@render}`) done — backed
  by an OXC bridge (`oxc_bridge.rs`) that converts OXC's estree output to the
  acorn/Svelte wire shape via position shifting, `loc` injection,
  ParenthesizedExpression unwrapping, TS empty-default stripping,
  leadingComments attachment, and TemplateElement-bound normalization
  (OXC includes the backtick / `${` markers; acorn doesn't).
- **2e** — all block kinds done: `{#if}`/`{:else if}`/`{:else}`/`{/if}`,
  `{#key}`, `{#each}` (with destructuring patterns), `{#snippet}` (with
  generic TS params), `{#await}` / `{:then}` / `{:catch}` chain.
- **2f** — `<script>` hoisting to `Root.instance/module` with parsed
  `Program` body, including the upstream quirk where the preceding HTML
  comment's text is reattached as `Program.leadingComments` (the
  `svelte-ignore` warning marker). `<svelte:options>` hoisting to
  `Root.options` with `runes` and `customElement.tag` extraction. All
  svelte:* meta tags routed to their dedicated AST variants
  (`SvelteBody/Boundary/Document/Fragment/Head/Options/Self/Window`).
- Attribute parsing: bare, quoted (with mustache interpolations),
  unquoted, `{...spread}`, `{name}` shorthand. `=/>` special-case for
  legacy `<a href=/>`. JS-style `//` and `/* */` comments between
  attributes pushed onto `Root.comments` with `loc.character` populated.
  Static-attribute path for `<script>`/`<style>` (suppresses mustache
  interpretation, allowing `<script generics="T extends { foo: number }">`).
- Directive parsing: all of `on:`/`bind:`/`use:`/`class:`/`style:`/
  `transition:`/`in:`/`out:`/`animate:`/`let:`. Quoted-string directive
  values (`on:click="{handler}"`) are unwrapped to the inner ExpressionTag,
  matching upstream's legacy form.
- Context-aware `<slot>` parsing: inside a `<template shadowrootmode>`
  ancestor, `<slot>` is a `RegularElement`, not a `SlotElement`.
- Pattern parsing (used by `{#each ... as PATTERN}` / `{:then PATTERN}` /
  `{:catch PATTERN}`) uses upstream's `(<pattern> = 1)` synthetic-source
  trick for `{...}`/`[...]` patterns; identifier patterns short-circuit to
  a hand-built `Identifier` (so their `loc` reflects original-source
  line/column via `LineMap`, matching `state.locator` in upstream).
- HTML entity decoding (`&amp;`, `&nbsp;`, `&quot;`, etc. plus decimal /
  hex numeric refs `&#NNN;` / `&#xHH;`). Attribute-value-specific rule for
  unterminated entities followed by `=` or alphanum.
- Component detection (`is_component_name`): uppercase ASCII start, or
  identifier-start with at least one `.` (e.g. `<Lib.Modal>`).
- `<svelte:component>` / `<svelte:element>` route to their dedicated
  variants with `this` attribute extracted to `expression` / `tag`.
- `<textarea>` body is parsed as a Text+ExpressionTag sequence (no nested
  elements); close tag uses the relaxed `</textarea(\s[^>]*)?>` regex.
- HTML implicit close: `<li><li>` closes the first `<li>` automatically;
  same for `<p>`, `<dt>`/`<dd>`, `<tr>`/`<td>`/`<th>`, etc. Uses
  `closing_tag_omitted` (ported from upstream `html-tree-validation.js`).
- JS comments inside mustache expressions: `{ /* comment */ a + b }` is
  parsed correctly via `skip_whitespace_and_js_comments` (which collects
  comments to `Root.comments` and skips them past `{`/before `}`).
**Phase 3** — in progress. Scope chain + binding classification are in
place; reference resolution, CSS analyze/prune/warn, and the 60+ validator
visitors are next.
- `svelte_analyze` provides `Analysis`, `Scope` (reference-counted, with
  parent + block-scope tracking), `Binding`, `ScopeRoot`, `BindingKind`,
  `DeclarationKind`.
- `analyze_component(root, filename)` walks both `<script>` blocks and:
  - builds nested scopes for function bodies, arrow functions, blocks,
    `for`/`for-in`/`for-of`, `try`/`catch`, `switch`, class bodies.
  - declares variables (`var` / `let` / `const` / function / class /
    import / `export` re-exports), walking destructuring patterns
    recursively. Variables hoist (`var` + function) are pre-declared before
    other statements.
  - classifies each binding by initializer: `let foo = $state(...)` →
    `State`; `$state.raw(...)` → `RawState`; `$derived(...) / .by(...)` →
    `Derived`; `let { foo } = $props()` → `Prop`; `let { ...rest } =
    $props()` → `RestProp`; `let { x = $bindable() } = $props()` →
    `BindableProp`. Uses a port of upstream's `get_rune` / `get_global_keypath`
    helpers from scope.js:1429-1480.
  - sets `runes: bool` if any rune call is present.
  - resolves every Identifier reference across the scope chain via
    `Scope::reference_chain` (matches the upstream `Scope.reference`
    algorithm: walks parents until a binding is found or the root is
    reached, attaching to `binding.references` along the way and to
    `ScopeRoot.conflicts` when unresolved).
- CSS analyze (port of `phases/2-analyze/css/css-analyze.js`): walks the
  parsed `StyleSheet` and tags each Rule / ComplexSelector / RelativeSelector
  with metadata. Identifies `:global(...)` and bare `:global` selectors as
  global, `:root` / `:host` / `::view-transition*` as global-like, `:global { }`
  block rules, and `@keyframes` declarations (with `-global-` prefix
  detection). Metadata is sidecar (keyed by `(start, end)`) — the CSS AST
  itself stays immutable. `Analysis.css_meta` exposes the maps; the prune
  pass reads them to decide which selectors to scope.
- CSS prune (full parity port of `phases/2-analyze/css/css-prune.js`):
  `template_elements::collect` builds an indexed tree of every renderable
  element with parent / prev / next sibling pointers, statically-known
  class / id / attribute values, plus an `Existence` value (Probable
  inside `{#if}` / `{#each}` / `{#await}`, Definite otherwise). Per-node
  `NodeKind` carries the upstream type discriminant so adjacent-sibling
  matching can special-case `Component` / `SlotElement` / `RenderTag` per
  css-prune.js:332-339. `css_prune::prune` walks each ComplexSelector
  through `apply_selector` → `apply_combinator`, mirrors all four
  combinators with PROB/DEF walk-through for `+`, recursive `:is` /
  `:where` / `:not` (with multi-chain scoping) / `:has` (with
  include_self in global contexts) / NestingSelector handling, the full
  `attribute_matches` (BindDirective / StyleDirective / ClassDirective /
  SpreadAttribute special-cases + `class` / `style` directive
  fall-throughs), `test_attribute` with all 6 operators + `i` /
  HTML5-case-insensitive attribute defaults +
  `whitelist_attribute_selector` (`<details open>`, `<dialog open>`),
  `gather_possible_values` for class={Literal | ConditionalExpression |
  LogicalExpression | ArrayExpression | ObjectExpression}, `truncate`
  of trailing `:global(...)`, implicit `&` injection for nested rules
  in `get_relative_selectors`, the `every_is_global` fallback,
  `has_definite_elements` gating, and `scoped_elements` tracking for
  the transform phase.
- CSS warn (port of `phases/2-analyze/css/css-warn.js`): emits a
  `css_unused_selector` warning for each ComplexSelector that the prune
  pass didn't mark `used`. Skips `:global { ... }` block preludes,
  `@keyframes` preludes, and the prelude of `:is(...)` / `:where(...)`.
- Template + JS validator (Phase 3h, partial): `validate::validate()` walks
  the template and `<script>` content emitting warnings/errors per node —
  mirrors `phases/2-analyze/visitors/`. ~26 visitors / checks ported:
  - **Special elements**: SvelteWindow / SvelteBody / SvelteDocument /
    SvelteHead / SvelteSelf / SvelteBoundary / SvelteFragment, TitleElement
  - **Tags**: HtmlTag, DebugTag, ConstTag (placement check)
  - **Blocks**: IfBlock / EachBlock / AwaitBlock / KeyBlock / SnippetBlock
    (`block_empty` warning + `snippet_invalid_rest_parameter` error)
  - **Directives**: LetDirective (parent-element check), StyleDirective
    (modifier check), OnDirective (runes-mode deprecation), BindDirective
    (DOM property catalog lookup with `valid_elements` /
    `invalid_elements` constraints from
    `phases/bindings.js` — full table of 50+ DOM bindings ported to
    `svelte_analyze::bindings::binding_properties()`)
  - **Components**: SvelteComponent (runes-mode deprecation)
  - **JS-side**: ImportDeclaration (forbidden `svelte/internal*` and
    `beforeUpdate`/`afterUpdate` from `svelte` in runes mode),
    LabeledStatement (legacy `$:` reactive statement → error in runes mode)
  
  `analyze_component()` returns `Err` on the first validation error,
  matching upstream's `InternalCompileError` fast-fail.

- Pending follow-ups: combinator-aware CSS prune polish; the remaining
  40-ish validator visitors; legacy
  `$:` reactive handling; identifier resolution inside template
  expressions (currently only walks `<script>` content).

- **2g** done. `svelte_css_parser` is a recursive-descent port of
  `read/style.js` (~650 LOC). Produces the full Svelte CSS AST:
  `StyleSheet`, `Atrule`, `Rule`, `SelectorList`, `ComplexSelector`,
  `RelativeSelector`, all `SimpleSelector` variants (Type/Id/Class/Attribute/
  PseudoElement/PseudoClass/Percentage/Nth/Nesting), `Combinator`, `Block`,
  `Declaration`. Handles `:nth-of-type(... of <selector-list>)`, `url(...)`,
  string-aware value reading, CSS escapes (`\HHHHHH` Unicode + `\<char>`),
  and `<!-- -->` / `/* */` comments. CSS AST types live in
  [`svelte_ast::css`](crates/svelte_ast/src/css.rs).

