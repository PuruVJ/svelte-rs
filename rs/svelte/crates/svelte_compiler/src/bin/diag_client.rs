// Quick diagnostic: compile a fixture for client and print the output (or
// the error code). Used to debug the typed-client pipeline.
use svelte_compiler::{compile, CompileOptions, Generate};

fn main() {
    let path = std::env::args().nth(1).expect("usage: diag_client FIXTURE.svelte");
    let source = std::fs::read_to_string(&path).expect("read");
    let ast = svelte_parse::parse(&source, false).expect("parse");
    let root = ast.root();
    eprintln!("=== Module script ===\n{:#?}", root.module.is_some());
    eprintln!("=== Instance script ===\n{:#?}", root.instance.is_some());
    eprintln!("=== Fragment ===\n{:#?}", root.fragment);
    let mut opts = CompileOptions::default();
    opts.module.generate = Some(Generate::Client);
    match compile(&source, "Diag", opts) {
        Ok(r) => {
            println!("=== OK ===\n{}", r.js);
        }
        Err(e) => {
            println!("=== ERR ===\n{:?}", e);
        }
    }
}
