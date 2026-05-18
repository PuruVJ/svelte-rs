fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("usage: dump_path <path>");
    let name = args.get(2).cloned().unwrap_or_else(|| "X".to_string());
    let src = std::fs::read_to_string(path).unwrap();
    let mut opts = svelte_compiler::CompileOptions::default();
    opts.module.generate = Some(svelte_compiler::Generate::Server);
    opts.module.experimental.async_ = true;
    match svelte_compiler::compile(&src, &name, opts) {
        Ok(r) => println!("{}", r.js),
        Err(e) => println!("ERR: {:?}", e),
    }
}
