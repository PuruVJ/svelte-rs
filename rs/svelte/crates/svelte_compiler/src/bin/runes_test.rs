use svelte_parse::parse;

fn main() {
    let src = "<svelte:options runes />\n<svelte:component this={A} />";
    let ast = parse(src, false).unwrap();
    let root = ast.root();
    println!("options: {:?}", root.options.as_ref().map(|o| (o.runes, o.custom_element.is_some())));
}
