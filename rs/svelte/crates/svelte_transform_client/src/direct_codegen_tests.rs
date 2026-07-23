#[cfg(test)]
mod tests {
    use svelte_parse::parse;
    use svelte_transform_shared::compile_bump::CompileBump;

    use crate::direct_codegen::{
        try_emit_client_program_direct, try_emit_fully_static_client_js,
    };
    use crate::typed_fast::try_typed_client;

    #[test]
    fn fully_static_direct_matches_program_shape() {
        let src = "<h1>hello world</h1>";
        let ast = parse(src, false).unwrap();
        let root = ast.root();
        let bump = CompileBump::new();
        let direct = try_emit_fully_static_client_js(&root, "Hello_world", &bump).unwrap();
        assert!(direct.contains("$.from_html(`<h1>hello world</h1>`)"));
        assert!(direct.contains("export default function Hello_world"));
        assert!(!direct.contains("print_typed"));
        assert!(try_typed_client(&root, "Hello_world", bump.bump()).is_some());
    }

    #[test]
    fn program_direct_for_static_program() {
        let src = "<h1>x</h1>";
        let ast = parse(src, false).unwrap();
        let root = ast.root();
        let bump = CompileBump::new();
        let program = try_typed_client(&root, "X", bump.bump()).unwrap();
        let js = try_emit_client_program_direct(&program).unwrap();
        assert!(js.contains("var root = $.from_html"));
        assert!(js.contains("$.append($$anchor"));
    }
}
