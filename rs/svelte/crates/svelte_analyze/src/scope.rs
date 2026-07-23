//! Scope chain + bindings.
//!
//! Ported from `packages/svelte/src/compiler/phases/scope.js`.
//!
//! The scope chain tracks variable declarations (`let`, `const`, `var`,
//! function/class declarations, imports, function parameters, snippet/each
//! block locals, template-bound names from `<input bind:value={x}>`, etc.)
//! and references between them. Every Identifier resolves to a Binding
//! through the scope it appears in (or falls back to a global / unknown
//! reference).
//!
//! This is the gravitational center of Phase 3; most downstream visitors
//! consume the scope chain to decide how to rewrite each expression.

use std::collections::HashMap;
use std::rc::{Rc, Weak};

use serde::{Deserialize, Serialize};
use svelte_js_ast::{Expression, Identifier};

/// What semantic role a binding plays in Svelte. Mirrors `BindingKind` in
/// `packages/svelte/src/compiler/types/index.d.ts:275-288`.
///
/// The default for a plain `let foo = ...` is `Normal`; runes / template
/// constructs (`$state`, `$props`, `$derived`, each-block locals, snippet
/// params, etc.) promote the kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingKind {
    /// A variable with no Svelte-specific role.
    Normal,
    /// A `let foo = $props()` destructured prop (read-only).
    Prop,
    /// A prop whose parent declared it via `$bindable()` (mutable, two-way).
    BindableProp,
    /// `let { ...rest } = $props()` — captures unused props.
    RestProp,
    /// `let foo = $state.raw(...)` — non-deep reactive state.
    RawState,
    /// `let foo = $state(...)` — deeply reactive state.
    State,
    /// `let foo = $derived(...)` / `$derived.by(...)`.
    Derived,
    /// `{#each items as item}` — `item` is `Each`.
    Each,
    /// `{#snippet foo(bar)}` — `bar` is `Snippet`.
    Snippet,
    /// `$store` — auto-subscribed value of a store.
    StoreSub,
    /// `$: legacy = reactive` (Svelte 4 reactivity).
    LegacyReactive,
    /// Template-bound binding: `{:then val}`, `{:catch err}`, `{@const}`, etc.
    Template,
    /// A binding whose value is statically known (e.g. each-block index).
    Static,
}

/// Mirrors `DeclarationKind` in
/// `packages/svelte/src/compiler/types/index.d.ts:290-303`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeclarationKind {
    Var,
    Let,
    Const,
    Using,
    #[serde(rename = "await using")]
    AwaitUsing,
    Function,
    Import,
    Param,
    RestParam,
    /// A binding the compiler synthesized after the fact (no source-level
    /// declaration). E.g. `$$props` / `$$restProps`.
    Synthetic,
}

/// One named binding in a scope. Mirrors `Binding` in scope.js:88-200.
///
/// Many fields are populated incrementally — `references` and `assignments`
/// are filled in by the Identifier visitor as it walks the program; `mutated`
/// / `reassigned` flip when those occur; rune-related metadata (e.g.
/// `prop_alias`) is filled when `$props()` / `$bindable()` is detected.
#[derive(Debug)]
pub struct Binding {
    pub name: String,
    pub kind: BindingKind,
    pub declaration_kind: DeclarationKind,
    /// Declaring `Identifier` node.
    pub node: Identifier,
    /// What the value was initialized with — for destructured props such as
    /// `let { foo = 'bar' } = $props()` this is `'bar'`, NOT `$props()`.
    pub initial: Option<Expression>,
    /// Every read reference (Identifier nodes that resolve to this binding).
    pub references: Vec<Reference>,
    /// Every write reference. Includes the initial declaration.
    pub assignments: Vec<Assignment>,
    pub reassigned: bool,
    pub mutated: bool,
    pub updated: bool,
    /// Alias used for renamed props: `{ class: klass } = $props()` →
    /// the binding `klass` has `prop_alias: Some("class")`.
    pub prop_alias: Option<String>,
    pub metadata: BindingMetadata,
}

#[derive(Debug, Clone)]
pub struct Reference {
    pub node: Identifier,
}

#[derive(Debug, Clone)]
pub struct Assignment {
    pub value: Expression,
    pub scope: WeakScope,
}

/// Per-binding analysis metadata.
#[derive(Debug, Default)]
pub struct BindingMetadata {
    pub inside_rest: bool,
}

/// A lexical scope. Mirrors `Scope` in scope.js:201-1471.
#[derive(Debug)]
pub struct Scope {
    pub root: WeakScopeRoot,
    pub parent: Option<WeakScope>,
    pub function_depth: u32,
    pub is_block_scope: bool,
    declarations: HashMap<String, Rc<RefCellBinding>>,
    references: HashMap<String, Vec<Reference>>,
}

/// `RefCell<Binding>` wrapper. Real impl exposes `&mut` access patterns —
/// for now we keep it as an alias so we can swap in `parking_lot::Mutex` /
/// `std::sync::Mutex` later if cross-thread access becomes needed.
pub type RefCellBinding = std::cell::RefCell<Binding>;

pub type ScopePtr = Rc<std::cell::RefCell<Scope>>;
pub type WeakScope = Weak<std::cell::RefCell<Scope>>;
pub type ScopeRootPtr = Rc<std::cell::RefCell<ScopeRoot>>;
pub type WeakScopeRoot = Weak<std::cell::RefCell<ScopeRoot>>;

/// The top-level container for a tree of scopes — one per compilation unit.
/// Mirrors `ScopeRoot` in scope.js:1100-1200ish.
#[derive(Debug, Default)]
pub struct ScopeRoot {
    /// Names already in use across the whole compilation unit. Used by the
    /// unique-name generator during transforms.
    pub conflicts: HashMap<String, u32>,
}

impl ScopeRoot {
    pub fn new() -> ScopeRootPtr {
        Rc::new(std::cell::RefCell::new(ScopeRoot::default()))
    }

    /// Allocate a name guaranteed to not conflict with anything currently
    /// known to this root. Mirrors `ScopeRoot.unique` in scope.js.
    pub fn unique(&mut self, preferred: &str) -> String {
        let n = self.conflicts.entry(preferred.to_string()).or_insert(0);
        let name = if *n == 0 {
            preferred.to_string()
        } else {
            format!("{preferred}_{n}")
        };
        *n += 1;
        name
    }
}

impl Scope {
    pub fn new_root(root: ScopeRootPtr, function_depth: u32) -> ScopePtr {
        Rc::new(std::cell::RefCell::new(Scope {
            root: Rc::downgrade(&root),
            parent: None,
            function_depth,
            is_block_scope: false,
            declarations: HashMap::new(),
            references: HashMap::new(),
        }))
    }

    pub fn child(parent: &ScopePtr, is_block_scope: bool) -> ScopePtr {
        let p = parent.borrow();
        Rc::new(std::cell::RefCell::new(Scope {
            root: p.root.clone(),
            parent: Some(Rc::downgrade(parent)),
            function_depth: p.function_depth + if is_block_scope { 0 } else { 1 },
            is_block_scope,
            declarations: HashMap::new(),
            references: HashMap::new(),
        }))
    }

    /// Declare a binding in this scope. Returns a handle to the binding for
    /// follow-up mutation (e.g. recording the initializer).
    pub fn declare(
        &mut self,
        name: String,
        kind: BindingKind,
        declaration_kind: DeclarationKind,
        node: Identifier,
    ) -> Rc<RefCellBinding> {
        let binding = Binding {
            name: name.to_string(),
            kind,
            declaration_kind,
            node,
            initial: None,
            references: Vec::new(),
            assignments: Vec::new(),
            reassigned: false,
            mutated: false,
            updated: false,
            prop_alias: None,
            metadata: BindingMetadata::default(),
        };
        let cell = Rc::new(std::cell::RefCell::new(binding));
        self.declarations.insert(name, Rc::clone(&cell));
        cell
    }

    /// Look up `name` in this scope only (no walking up to the parent).
    pub fn get_local(&self, name: &str) -> Option<Rc<RefCellBinding>> {
        self.declarations.get(name).map(Rc::clone)
    }

    /// Walk up the scope chain looking for a binding named `name`.
    pub fn get(scope: &ScopePtr, name: &str) -> Option<Rc<RefCellBinding>> {
        let s = scope.borrow();
        if let Some(b) = s.declarations.get(name).map(Rc::clone) {
            return Some(b);
        }
        let parent_weak = s.parent.clone();
        drop(s);
        if let Some(parent_weak) = parent_weak {
            if let Some(parent) = parent_weak.upgrade() {
                return Scope::get(&parent, name);
            }
        }
        None
    }

    /// Names declared in *this* scope (not parents). Iteration order isn't
    /// stable — callers that need stable order should sort.
    pub fn local_names(&self) -> impl Iterator<Item = &String> {
        self.declarations.keys()
    }

    /// Record a reference and bubble it up to the binding's scope if found.
    /// Mirrors `Scope.reference` in scope.js:776-794.
    ///
    /// Called as `Scope::reference_chain(&scope, "foo", reference)` because
    /// it walks `self.parent` and would conflict with `&mut self` reborrow
    /// rules. The walk attaches `reference` to:
    /// - this scope's `references[name]` (always),
    /// - the binding's `references` list once we find the scope that
    ///   declares `name`,
    /// - the `ScopeRoot.conflicts` counter if no binding is found (global).
    pub fn reference_chain(scope: &ScopePtr, name: String, reference: Reference) {
        let mut s_ptr = Rc::clone(scope);
        let mut first = true;
        loop {
            // Step 1: buffer on this scope's references map.
            {
                let mut s = s_ptr.borrow_mut();
                s.references
                    .entry(name.to_string())
                    .or_default()
                    .push(reference.clone());
                // Step 2: try to attach to a binding here.
                if let Some(b) = s.declarations.get(&name) {
                    b.borrow_mut().references.push(reference.clone());
                    return;
                }
                let _ = first;
                first = false;
            }
            // Step 3: ascend.
            let next = {
                let s = s_ptr.borrow();
                s.parent.as_ref().and_then(|w| w.upgrade())
            };
            match next {
                Some(p) => s_ptr = p,
                None => {
                    // Reached root with no match — record as a global.
                    let s = s_ptr.borrow();
                    if let Some(root_rc) = s.root.upgrade() {
                        *root_rc
                            .borrow_mut()
                            .conflicts
                            .entry(name.to_string())
                            .or_insert(0) += 1;
                    }
                    return;
                }
            }
        }
    }

    /// Single-scope reference buffering (legacy entry point — does NOT walk
    /// the parent chain or attach to a binding). Kept for places where the
    /// caller wants only local buffering.
    pub fn reference(&mut self, name: String, reference: Reference) {
        self.references.entry(name).or_default().push(reference);
    }
}
