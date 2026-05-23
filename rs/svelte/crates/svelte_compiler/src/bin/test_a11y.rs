use svelte_parse::parse;
use svelte_analyze::analyze_component;

fn main() {
    let path = std::env::args().nth(1).expect("path");
    let src = std::fs::read_to_string(&path).unwrap();
    let root = parse(&src, false).unwrap();
    println!("runes: {:?}", root.options);
    println!("instance: {}", root.instance.is_some());
    if let Some(s) = &root.instance {
        println!("instance decls: {}", s.content.body.len());
    }
    let analysis = analyze_component(&root, None).unwrap();
    println!("is_runes: {}", analysis.runes);
    println!("Warnings: {}", analysis.warnings.len());
    for w in &analysis.warnings {
        println!("  [{}] @ {:?}", w.code, w.position);
    }
}
