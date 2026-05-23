//! Per-`compile()` bump arena: template AST + scratch buffers for direct JS emission.

use bumpalo::Bump;
pub use svelte_ast::arena::TemplateArena;

/// Bump allocator scoped to a single `compile()` invocation.
///
/// Template nodes and direct-codegen scratch strings share one [`Bump`] so the
/// whole compile frees in one drop.
pub struct CompileBump {
    pub template: TemplateArena,
}

impl CompileBump {
    pub fn new() -> Self {
        Self {
            template: TemplateArena::new(),
        }
    }

    pub fn bump(&self) -> &Bump {
        &self.template.bump
    }

    /// Growable string backed by the arena (finalized with [`BumpString::into_owned`]).
    pub fn string(&self) -> BumpString<'_> {
        BumpString::new_in(&self.template.bump)
    }
}

/// Arena-backed string builder; copies out once at the end.
pub struct BumpString<'a> {
    inner: bumpalo::collections::String<'a>,
}

impl<'a> BumpString<'a> {
    pub fn new_in(bump: &'a Bump) -> Self {
        Self {
            inner: bumpalo::collections::String::new_in(bump),
        }
    }

    pub fn push_str(&mut self, s: &str) {
        self.inner.push_str(s);
    }

    pub fn push(&mut self, c: char) {
        self.inner.push(c);
    }

    pub fn as_str(&self) -> &str {
        self.inner.as_str()
    }

    pub fn into_owned(self) -> String {
        self.inner.to_string()
    }
}
