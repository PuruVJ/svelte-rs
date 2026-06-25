fn main() {
    let src = std::fs::read_to_string("/Users/puruvijay/Projects/svelte-rs/packages/svelte/tests/snapshot/samples/async-top-level-group-sync-run/index.svelte").unwrap();
    let root = svelte_parse::parse(&src, false).unwrap();
    eprintln!("Root.comments: {}", root.comments.len());
    for c in &root.comments {
        eprintln!("  [{}..{}] {:?} {:?}", c.start, c.end, c.kind, c.value);
    }
    if let Some(s) = &root.instance {
        eprintln!("Instance content.comments: ?? (no field)");
        eprintln!("Instance start..end: {}..{}", s.start, s.end);
    }
    if let Some(s) = &root.instance {
        for stmt in &s.content.body {
            if let svelte_js_ast::Statement::Variable(v) = stmt {
                for d in &v.declarations {
                    if let svelte_js_ast::Pattern::Identifier(id) = &d.id {
                        eprintln!("Var '{}' span: {}..{}", id.name, id.span.start, id.span.end);
                    }
                }
            }
        }
    }
}
