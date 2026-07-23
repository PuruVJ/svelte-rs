//! Sanity-check: after Phase D hoist, scripts populate `Root.instance` with
//! a typed Program containing real statements.

use svelte_compiler::ParseOptions;

#[test]
#[ignore]
fn imports_in_modules_script_is_typed() {
    let svelte = "<script>\n\timport { random } from './module.svelte';\n</script>\n";
    let ast = svelte_compiler::parse(svelte, ParseOptions::default()).expect("parse");
    let root = ast.root();

    let instance = root.instance.as_ref().expect("instance script present");
    assert_eq!(instance.context, svelte_ast::ScriptContext::Default);
    assert_eq!(instance.content.body.len(), 1, "expected 1 statement, got {}", instance.content.body.len());
    match &instance.content.body[0] {
        svelte_js_ast::Statement::Import(d) => {
            assert_eq!(d.source.value, "./module.svelte");
            assert_eq!(d.specifiers.len(), 1);
            match &d.specifiers[0] {
                svelte_js_ast::ImportSpecifierKind::Named(s) => {
                    assert_eq!(s.local.name, "random");
                }
                _ => panic!("expected named specifier"),
            }
        }
        other => panic!("expected ImportDeclaration, got {other:?}"),
    }
}

#[test]
#[ignore]
fn module_script_lands_in_module() {
    let svelte = "<script context=\"module\">\n\texport const meta = 'x';\n</script>\n";
    let ast = svelte_compiler::parse(svelte, ParseOptions::default()).expect("parse");
    let root = ast.root();
    assert!(root.module.is_some(), "module script should populate Root.module");
    assert!(root.instance.is_none(), "no instance script");
}

#[test]
#[ignore]
fn style_block_populates_root_css() {
    let svelte = "<p>hi</p>\n<style>\n\tp { color: red; }\n</style>\n";
    let ast = svelte_compiler::parse(svelte, ParseOptions::default()).expect("parse");
    let root = ast.root();
    assert!(root.css.is_some(), "<style> should populate Root.css");
}
