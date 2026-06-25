//! `Analysis` struct — the output of Phase 2.
//!
//! Ported from `packages/svelte/src/compiler/phases/2-analyze/types.d.ts`.
//! Holds everything the transform phases need: the scope chain, the runes
//! detected, the parsed CSS StyleSheet, and a collection of warnings + errors
//! that surfaced during analysis.

use std::collections::HashMap;
use std::rc::Rc;

use svelte_ast::Root;
use svelte_diagnostics::CompileDiagnostic;

use crate::css_analyze::CssAnalysis;
use crate::scope::{ScopePtr, ScopeRootPtr};

/// Output of `analyze_component` / `analyze_module`. Consumed by the
/// transform phases (client / server).
#[derive(Debug)]
pub struct Analysis {
    /// The parsed template (same as the input).
    pub root: Root,
    /// Scope of the instance script (or root scope if no `<script>`).
    pub instance: ScopePtr,
    /// Scope of the module script (or root scope if no `<script module>`).
    pub module: ScopePtr,
    pub scope_root: ScopeRootPtr,
    /// True if any rune (`$state`, `$derived`, `$effect`, `$props`,
    /// `$bindable`, `$inspect`, `$host`) was detected in the instance or
    /// module script.
    pub runes: bool,
    /// Sidecar metadata for the parsed CSS, populated by `analyze_css`.
    /// Each Rule / ComplexSelector / RelativeSelector is tagged with its
    /// global / global-like / scoped state.
    pub css_meta: CssAnalysis,
    /// Component filename, if known.
    pub filename: Option<String>,
    /// Component name (derived from filename). Used in error messages and
    /// as the default JS class name.
    pub name: String,
    /// Map of `style-prop-name -> hash` for CSS scoping. Empty until CSS
    /// analyze runs.
    pub css_hash: String,
    pub warnings: Vec<CompileDiagnostic>,
    /// Element-side metadata gathered during analysis (e.g. which props
    /// referenced which bindings). Mirrors `Analysis.elements` in upstream.
    pub elements: HashMap<String, ElementMetadata>,
    /// Whether any `<style>` block uses `:global` selectors.
    pub uses_global: bool,
    /// Whether the component uses `await` at the top level (Svelte 5.36+
    /// `experimental.async`).
    pub uses_async: bool,
    /// Set of names of `<script>` exports — used to detect prop renames and
    /// reserved-name conflicts.
    pub exports: Vec<String>,
}

impl Analysis {
    /// True if the component's CSS contains any unscoped (`:global`)
    /// rules or `-global-` keyframes.
    pub fn has_global_css(&self) -> bool {
        self.css_meta.has_global
    }
}

#[derive(Debug, Default)]
pub struct ElementMetadata {
    /// Bindings used in this element's `bind:`, `class:`, `style:` etc.
    /// Mirrors the metadata upstream collects per-element.
    pub used_bindings: Vec<Rc<crate::scope::RefCellBinding>>,
}
