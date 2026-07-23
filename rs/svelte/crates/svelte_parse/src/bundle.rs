//! Owns the template bump arena and the parsed [`Root`] together via `self_cell`.

use self_cell::self_cell;
use svelte_ast::{Root, TemplateArena};
use svelte_diagnostics::CompileDiagnostic;

use crate::parse_in_arena;

self_cell!(
    /// Parsed template AST bundled with its bump arena.
    pub struct AstBundle {
        owner: TemplateArena,
        #[covariant]
        dependent: Root,
    }
);

impl AstBundle {
    /// Parse `source` into a new arena-backed [`Root`].
    pub fn try_parse(source: &str, loose: bool) -> Result<Self, CompileDiagnostic> {
        Self::try_new(TemplateArena::new(), |arena| parse_in_arena(arena, source, loose))
    }

    pub fn root(&self) -> &Root<'_> {
        self.borrow_dependent()
    }

    pub fn with_root_mut<R>(&mut self, f: impl FnOnce(&mut Root<'_>) -> R) -> R {
        self.with_dependent_mut(|_owner, root| f(root))
    }

    pub fn arena(&self) -> &TemplateArena {
        self.borrow_owner()
    }

    /// Consume and return the owned arena (drops the AST).
    pub fn into_arena(self) -> TemplateArena {
        self.into_owner()
    }
}

/// Back-compat alias for code expecting a “parsed component” type name.
pub type ParsedComponent = AstBundle;
