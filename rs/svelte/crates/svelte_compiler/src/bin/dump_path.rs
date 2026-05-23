fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("usage: dump_path <path> [name] [server|client]");
    let name = args.get(2).cloned().unwrap_or_else(|| "X".to_string());
    let mode = args.get(3).map(|s| s.as_str()).unwrap_or("server");
    let src = std::fs::read_to_string(path).unwrap();
    let mut opts = svelte_compiler::CompileOptions::default();
    opts.module.generate = Some(if mode == "client" {
        svelte_compiler::Generate::Client
    } else {
        svelte_compiler::Generate::Server
    });
    opts.module.experimental.async_ = true;
    opts.module.filename = Some(path.clone());
    opts.name = Some(name);
    match svelte_compiler::compile(&src, opts) {
        Ok(r) => println!("{}", r.js.code),
        Err(e) => println!("ERR: {:?}", e),
    }
}
