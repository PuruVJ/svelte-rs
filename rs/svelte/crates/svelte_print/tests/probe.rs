use svelte_parse::parse;
use svelte_print::{print, PrintOptions};

#[test]
fn probe_html_tag() {
    let src = "<article>\n\t{@html content}\n</article>";
    let root = parse(src, false).unwrap();
    eprintln!("AST nodes: {:#?}", root.fragment.nodes);
    let out = print(&root, PrintOptions::default());
    eprintln!("---OUT---\n{}", out.code);
}
