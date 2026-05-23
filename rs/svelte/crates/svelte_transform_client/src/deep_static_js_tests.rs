#[cfg(test)]
mod tests {
    use svelte_parse::parse;
    use svelte_transform_shared::compile_bump::CompileBump;

    use crate::deep_static_js::try_emit_deep_static_walker_js;
    use crate::script_fast::analyze_script_props_only;
    use crate::walker::scan_fragment_assignments;

    #[test]
    fn skip_static_subtree_direct_js() {
        let path = "/workspace/packages/svelte/tests/snapshot/samples/skip-static-subtree/index.svelte";
        let source = std::fs::read_to_string(path).unwrap();
        let mut root = parse(&source, false).unwrap();
        svelte_transform_shared::template_meta::mark_template_metadata(&mut root);
        crate::static_html_cache::precompute_static_html_cache(&mut root);

        let assigned = scan_fragment_assignments(&root.fragment);
        let script = analyze_script_props_only(root.instance.as_ref(), &assigned).unwrap();
        let bump = CompileBump::new();
        let js = try_emit_deep_static_walker_js(&root.fragment, "Skip_static_subtree", &script, &bump)
            .expect("deep static direct emit");

        assert!(js.contains("$.from_html(`"));
        assert!(js.contains("$.html(node, () => $$props.content)"));
        assert!(js.contains("$.next(14)"));
        assert!(js.contains("$.set_custom_element_data(custom_elements, 'with', 'attributes')"));
        assert!(js.contains("$.template_effect(() => $.set_text(text, $$props.title))"));
        assert!(js.contains("export default function Skip_static_subtree"));
    }
}
