//! Scoped CSS hash resolution — mirrors upstream `options.cssHash(...)`.

use svelte_ast::Root;
use svelte_transform_shared::str_hash::{default_css_class_hash, svelte_str_hash};

use crate::options::CompileOptions;

/// Resolve the scoped CSS class hash for this compile.
///
/// When `options.css_hash` is set (from the JS bridge after calling a
/// `cssHash` function), that value wins. Otherwise the upstream default
/// `svelte-${hash(filename ?? css)}` applies.
pub fn resolve_css_hash(options: &CompileOptions, source: &str, root: &Root<'_>) -> Option<String> {
    let sheet = root.css.as_ref()?;
    if let Some(ref hash) = options.css_hash {
        return Some(hash.clone());
    }
    let filename = options.module.filename.as_deref().unwrap_or("(unknown)");
    let css_styles = css_styles_text(source, sheet);
    Some(default_css_class_hash(filename, &css_styles))
}

fn css_styles_text(source: &str, sheet: &svelte_ast::css::StyleSheet<'_>) -> String {
    if !sheet.content.styles.is_empty() {
        return sheet.content.styles.clone();
    }
    let start = sheet.content.start as usize;
    let end = sheet.content.end as usize;
    if end <= source.len() && start <= end {
        source[start..end].to_string()
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use svelte_parse::parse_in_arena;
    use svelte_transform_shared::compile_bump::CompileBump;

    #[test]
    fn default_hash_uses_filename() {
        let compile_bump = CompileBump::new();
        let source = "<style>.x{color:red}</style><p></p>";
        let root = parse_in_arena(&compile_bump.template, source, false).unwrap();
        let mut opts = CompileOptions::default();
        opts.module.filename = Some("Foo.svelte".into());
        let hash = resolve_css_hash(&opts, source, &root).unwrap();
        assert_eq!(hash, default_css_class_hash("Foo.svelte", ".x{color:red}"));
    }

    #[test]
    fn override_from_options() {
        let compile_bump = CompileBump::new();
        let source = "<style>.x{}</style>";
        let root = parse_in_arena(&compile_bump.template, source, false).unwrap();
        let mut opts = CompileOptions::default();
        opts.css_hash = Some("svelte-xyz".into());
        assert_eq!(
            resolve_css_hash(&opts, source, &root).as_deref(),
            Some("svelte-xyz")
        );
    }

    #[test]
    fn str_hash_empty_string() {
        assert_eq!(svelte_str_hash(""), "45h");
    }
}
