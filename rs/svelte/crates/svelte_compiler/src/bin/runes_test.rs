use svelte_parse::parse;

fn main() {
    let src = "<svelte:options runes />\n<svelte:component this={A} />";
    let root = parse(src, false).unwrap();
    println!("options: {:?}", root.options.as_ref().map(|o| (o.runes, o.custom_element.is_some())));
}
