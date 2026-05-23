//! Bump arena helpers for template AST nodes.
//!
//! Template trees are allocated into a single [`Bump`] per compile/parse so the
//! whole AST drops in one free. Script `Program.body` shares the same bump;
//! nested `Statement` / `Expression` nodes remain heap-owned with `Clone`.

use bumpalo::Bump;

/// Per-parse / per-compile bump allocator for template data.
pub struct TemplateArena {
    pub bump: Bump,
}

impl TemplateArena {
    pub fn new() -> Self {
        Self {
            bump: Bump::new(),
        }
    }

    pub fn alloc_str<'a>(&'a self, s: &str) -> &'a str {
        self.bump.alloc_str(s)
    }

    pub fn alloc_string<'a>(&'a self, s: impl Into<bumpalo::collections::String<'a>>) -> bumpalo::collections::String<'a> {
        s.into()
    }

    pub fn vec<'a, T>(&'a self) -> bumpalo::collections::Vec<'a, T> {
        bumpalo::collections::Vec::new_in(&self.bump)
    }

    pub fn boxed<'a, T>(&'a self, value: T) -> bumpalo::boxed::Box<'a, T> {
        bumpalo::boxed::Box::new_in(value, &self.bump)
    }
}

impl Default for TemplateArena {
    fn default() -> Self {
        Self::new()
    }
}
