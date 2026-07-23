// Quick diagnostic: compile a fixture for server and print the output (or
// the error code).
use svelte_compiler::{compile, CompileOptions, Generate};

fn main() {
    let path = std::env::args().nth(1).expect("usage: diag_server FIXTURE.svelte");
    let source = std::fs::read_to_string(&path).expect("read");
    let mut opts = CompileOptions::default();
    opts.module.generate = Some(Generate::Server);
    let fixture_dir = std::path::Path::new(&path).parent().unwrap();
    let cfg = std::fs::read_to_string(fixture_dir.join("_config.js")).unwrap_or_default();
    if cfg.contains("async: true") {
        opts.module.experimental.async_ = true;
    }
    // The SSR test driver always sets experimental.async = true.
    if path.contains("server-side-rendering") {
        opts.module.experimental.async_ = true;
    }
    opts.name = Some("Main".to_string());
    match compile(&source, opts) {
        Ok(r) => println!("=== OK ===\n{}", r.js.code),
        Err(e) => println!("=== ERR ===\n{:?}", e),
    }
}
