use svelte_parse::parse;
use svelte_analyze::{analyze_component, css_render::render_stylesheet};

fn main() {
    let path = std::env::args().nth(1).expect("path");
    let src = std::fs::read_to_string(&path).unwrap();
    let root = parse(&src, false).unwrap();
    if std::env::args().any(|a| a == "--dbg-attrs") {
        for c in &root.fragment.nodes {
            walk_attrs(c);
        }
    }
    let dbg = std::env::args().any(|a| a == "--dbg-classes");
    let root_clone = root.clone();
    let mut analysis = analyze_component(&root, None).unwrap();
    analysis.css_hash = "svelte-xyz".to_string();
    if dbg {
        let elements = svelte_analyze::template_elements::collect(&root_clone.fragment);
        for (i, el) in elements.elements.iter().enumerate() {
            eprintln!(
                "el {} tag={:?} kind={:?} classes_known={:?} unknown={}",
                i, el.tag, el.kind, el.classes.known, el.classes.unknown
            );
        }
    }
    if let Some(sheet) = analysis.css.as_ref() {
        let out = render_stylesheet(&src, sheet, &analysis.css_meta, "svelte-xyz");
        println!("=={}==\n{}", path, out);
    }
}

fn walk_attrs(n: &svelte_ast::FragmentChild) {
    if let svelte_ast::FragmentChild::RegularElement(el) = n {
        for a in &el.attributes {
            if let svelte_ast::ElementAttribute::Attribute(attr) = a {
                eprintln!("attr {} = {:#?}", attr.name, attr.value);
            }
        }
        for c in &el.fragment.nodes {
            walk_attrs(c);
        }
    }
}
