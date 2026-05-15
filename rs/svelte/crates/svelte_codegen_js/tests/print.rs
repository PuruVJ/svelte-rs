//! Integration tests for `svelte_codegen_js`.
//!
//! Each test constructs an acorn-shaped AST as JSON and verifies the emitted
//! code matches the expected JavaScript output.

use serde_json::json;
use svelte_codegen_js::{default_visitors, print, PrintOptions};

fn render(node: &serde_json::Value) -> String {
    let v = default_visitors();
    print(node, &v, &PrintOptions::default()).code
}

fn render_with_comments(node: &serde_json::Value, comments: Vec<serde_json::Value>) -> String {
    let v = default_visitors();
    let opts = PrintOptions {
        comments,
        ..PrintOptions::default()
    };
    print(node, &v, &opts).code
}

#[test]
fn const_declaration_with_number() {
    let ast = json!({
        "type": "Program",
        "body": [{
            "type": "VariableDeclaration",
            "kind": "const",
            "declarations": [{
                "type": "VariableDeclarator",
                "id": { "type": "Identifier", "name": "x" },
                "init": { "type": "Literal", "raw": "1" }
            }]
        }]
    });
    assert_eq!(render(&ast), "const x = 1;");
}

#[test]
fn binary_expression_renders_with_precedence() {
    let ast = json!({
        "type": "BinaryExpression",
        "operator": "+",
        "left": { "type": "Literal", "raw": "1" },
        "right": {
            "type": "BinaryExpression",
            "operator": "*",
            "left": { "type": "Literal", "raw": "2" },
            "right": { "type": "Literal", "raw": "3" }
        }
    });
    // 1 + 2 * 3 — no parens needed because `*` > `+`
    assert_eq!(render(&ast), "1 + 2 * 3");
}

#[test]
fn binary_expression_wraps_lower_precedence_child() {
    let ast = json!({
        "type": "BinaryExpression",
        "operator": "*",
        "left": {
            "type": "BinaryExpression",
            "operator": "+",
            "left": { "type": "Literal", "raw": "1" },
            "right": { "type": "Literal", "raw": "2" }
        },
        "right": { "type": "Literal", "raw": "3" }
    });
    // (1 + 2) * 3 — left needs parens
    assert_eq!(render(&ast), "(1 + 2) * 3");
}

#[test]
fn call_with_member_callee() {
    let ast = json!({
        "type": "CallExpression",
        "callee": {
            "type": "MemberExpression",
            "object": { "type": "Identifier", "name": "console" },
            "property": { "type": "Identifier", "name": "log" },
            "computed": false
        },
        "arguments": [
            { "type": "Literal", "raw": "\"hi\"" }
        ]
    });
    assert_eq!(render(&ast), "console.log(\"hi\")");
}

#[test]
fn arrow_single_param_with_parens() {
    // Upstream esrap always parenthesizes arrow params, even when there's
    // exactly one Identifier (`ts/index.js:859`).
    let ast = json!({
        "type": "ArrowFunctionExpression",
        "async": false,
        "params": [{ "type": "Identifier", "name": "x" }],
        "body": {
            "type": "BinaryExpression",
            "operator": "*",
            "left": { "type": "Identifier", "name": "x" },
            "right": { "type": "Literal", "raw": "2" }
        }
    });
    assert_eq!(render(&ast), "(x) => x * 2");
}

#[test]
fn arrow_multi_param_uses_parens() {
    let ast = json!({
        "type": "ArrowFunctionExpression",
        "async": false,
        "params": [
            { "type": "Identifier", "name": "a" },
            { "type": "Identifier", "name": "b" }
        ],
        "body": {
            "type": "BinaryExpression",
            "operator": "+",
            "left": { "type": "Identifier", "name": "a" },
            "right": { "type": "Identifier", "name": "b" }
        }
    });
    assert_eq!(render(&ast), "(a, b) => a + b");
}

#[test]
fn if_else_renders() {
    let ast = json!({
        "type": "IfStatement",
        "test": { "type": "Identifier", "name": "ok" },
        "consequent": {
            "type": "BlockStatement",
            "body": [{
                "type": "ExpressionStatement",
                "expression": {
                    "type": "CallExpression",
                    "callee": { "type": "Identifier", "name": "yes" },
                    "arguments": []
                }
            }]
        },
        "alternate": {
            "type": "BlockStatement",
            "body": [{
                "type": "ExpressionStatement",
                "expression": {
                    "type": "CallExpression",
                    "callee": { "type": "Identifier", "name": "no" },
                    "arguments": []
                }
            }]
        }
    });
    let expected = "if (ok) {\n\tyes();\n} else {\n\tno();\n}";
    assert_eq!(render(&ast), expected);
}

#[test]
fn import_default_and_named() {
    let ast = json!({
        "type": "ImportDeclaration",
        "specifiers": [
            {
                "type": "ImportDefaultSpecifier",
                "local": { "type": "Identifier", "name": "React" }
            },
            {
                "type": "ImportSpecifier",
                "imported": { "type": "Identifier", "name": "useState" },
                "local": { "type": "Identifier", "name": "useState" }
            }
        ],
        "source": { "type": "Literal", "raw": "\"react\"" }
    });
    assert_eq!(render(&ast), "import React, { useState } from \"react\";");
}

#[test]
fn template_literal_with_expression() {
    let ast = json!({
        "type": "TemplateLiteral",
        "quasis": [
            { "value": { "raw": "hello, " } },
            { "value": { "raw": "!" } }
        ],
        "expressions": [{ "type": "Identifier", "name": "name" }]
    });
    assert_eq!(render(&ast), "`hello, ${name}!`");
}

#[test]
fn export_default_arrow() {
    let ast = json!({
        "type": "ExportDefaultDeclaration",
        "declaration": {
            "type": "ArrowFunctionExpression",
            "async": false,
            "params": [],
            "body": { "type": "Literal", "raw": "42" }
        }
    });
    assert_eq!(render(&ast), "export default () => 42;");
}

/// Leading line comment before a top-level statement is emitted on its own line
/// followed by the statement. Comment loc is line 1 col 0; statement loc is line 2 col 0.
#[test]
fn leading_line_comment_before_statement() {
    let ast = json!({
        "type": "Program",
        "loc": {
            "start": { "line": 1, "column": 0 },
            "end": { "line": 2, "column": 5 }
        },
        "body": [{
            "type": "VariableDeclaration",
            "kind": "const",
            "loc": {
                "start": { "line": 2, "column": 0 },
                "end": { "line": 2, "column": 11 }
            },
            "declarations": [{
                "type": "VariableDeclarator",
                "id": { "type": "Identifier", "name": "x" },
                "init": { "type": "Literal", "raw": "1" }
            }]
        }]
    });
    let comments = vec![json!({
        "type": "Line",
        "value": " comment",
        "loc": {
            "start": { "line": 1, "column": 0 },
            "end": { "line": 1, "column": 10 }
        }
    })];
    let out = render_with_comments(&ast, comments);
    // Leading comment is flushed via flush_comments_until in emit_body's pre-pass.
    // The comment ends on line 1, the statement begins on line 2 → newline between.
    assert_eq!(out, "// comment\nconst x = 1;");
}

/// Trailing line comment on the same line as a statement gets emitted after
/// the statement, on the same line. (TODO: requires `loc` info on the
/// VariableDeclaration AND the cursor being correctly positioned.)
#[test]
fn trailing_line_comment_same_line() {
    let ast = json!({
        "type": "Program",
        "loc": {
            "start": { "line": 1, "column": 0 },
            "end": { "line": 1, "column": 30 }
        },
        "body": [{
            "type": "VariableDeclaration",
            "kind": "const",
            "loc": {
                "start": { "line": 1, "column": 0 },
                "end": { "line": 1, "column": 11 }
            },
            "declarations": [{
                "type": "VariableDeclarator",
                "id": { "type": "Identifier", "name": "x" },
                "init": { "type": "Literal", "raw": "1" }
            }]
        }]
    });
    let comments = vec![json!({
        "type": "Line",
        "value": " trailing",
        "loc": {
            "start": { "line": 1, "column": 13 },
            "end": { "line": 1, "column": 24 }
        }
    })];
    let out = render_with_comments(&ast, comments);
    // Trailing line comment is emitted with a leading space; pending newline
    // doesn't flush because nothing follows. Mirrors esrap behavior: newlines
    // are layout commands, not literal `\n` bytes (`index.js:121-128`).
    assert_eq!(out, "const x = 1; // trailing");
}

/// Call-argument list emission mirrors upstream esrap's last-arg-special-case
/// logic: the multi-line decision is based on non-last arguments only. A
/// single-line list of identifiers stays on one line regardless of total
/// length — only inherently multi-line non-last args force a wrap.
#[test]
fn call_args_with_no_multiline_children_stay_on_one_line() {
    let mk = |name: &str| json!({ "type": "Identifier", "name": name });
    let ast = json!({
        "type": "CallExpression",
        "callee": { "type": "Identifier", "name": "fn" },
        "arguments": [
            mk("argument_number_one"),
            mk("argument_number_two"),
            mk("argument_number_three"),
            mk("argument_number_four")
        ]
    });
    assert_eq!(
        render(&ast),
        "fn(argument_number_one, argument_number_two, argument_number_three, argument_number_four)"
    );
}

/// Final argument multi-line: e.g. object/array literal as the last arg. The
/// outer call stays single-line on the opening side because non-last args
/// aren't multi-line.
#[test]
fn call_with_multiline_final_arg_does_not_break_outer() {
    let mk_obj = json!({
        "type": "ObjectExpression",
        "properties": [
            { "type": "Property", "kind": "init", "key": {"type":"Identifier","name":"a"}, "value": {"type":"Literal","raw":"1"}, "computed": false, "shorthand": false, "method": false },
            { "type": "Property", "kind": "init", "key": {"type":"Identifier","name":"b"}, "value": {"type":"Literal","raw":"2"}, "computed": false, "shorthand": false, "method": false }
        ]
    });
    let ast = json!({
        "type": "CallExpression",
        "callee": { "type": "Identifier", "name": "f" },
        "arguments": [{ "type": "Identifier", "name": "x" }, mk_obj]
    });
    // Non-last arg is just `x`; last arg is `{ a: 1, b: 2 }` which is a single-line
    // object expression, so the outer call stays on one line.
    assert_eq!(render(&ast), "f(x, { a: 1, b: 2 })");
}

/// Short arg lists stay single-line.
#[test]
fn short_call_args_stay_single_line() {
    let ast = json!({
        "type": "CallExpression",
        "callee": { "type": "Identifier", "name": "fn" },
        "arguments": [
            { "type": "Identifier", "name": "a" },
            { "type": "Identifier", "name": "b" }
        ]
    });
    assert_eq!(render(&ast), "fn(a, b)");
}

/// Object expressions render with surrounding spaces inside `{ ... }`.
#[test]
fn object_with_property_renders_with_spaces() {
    let ast = json!({
        "type": "ObjectExpression",
        "properties": [{
            "type": "Property",
            "kind": "init",
            "shorthand": false,
            "computed": false,
            "method": false,
            "key": { "type": "Identifier", "name": "ok" },
            "value": { "type": "Literal", "raw": "true" }
        }]
    });
    assert_eq!(render(&ast), "{ ok: true }");
}

/// Mirror of `packages/svelte/tests/snapshot/samples/imports-in-modules/_expected/server/index.svelte.js`.
/// Tests adjacent-imports stay packed (no blank line between), but imports →
/// export default gets a margin.
#[test]
fn snapshot_imports_in_modules_server() {
    let ast = json!({
        "type": "Program",
        "body": [
            {
                "type": "ImportDeclaration",
                "specifiers": [{
                    "type": "ImportNamespaceSpecifier",
                    "local": { "type": "Identifier", "name": "$" }
                }],
                "source": { "type": "Literal", "raw": "'svelte/internal/server'" }
            },
            {
                "type": "ImportDeclaration",
                "specifiers": [{
                    "type": "ImportSpecifier",
                    "imported": { "type": "Identifier", "name": "random" },
                    "local": { "type": "Identifier", "name": "random" }
                }],
                "source": { "type": "Literal", "raw": "'./module.svelte'" }
            },
            {
                "type": "ExportDefaultDeclaration",
                "declaration": {
                    "type": "FunctionDeclaration",
                    "async": false,
                    "generator": false,
                    "id": { "type": "Identifier", "name": "Imports_in_modules" },
                    "params": [{ "type": "Identifier", "name": "$$renderer" }],
                    "body": { "type": "BlockStatement", "body": [] }
                }
            }
        ]
    });
    let expected = "import * as $ from 'svelte/internal/server';\nimport { random } from './module.svelte';\n\nexport default function Imports_in_modules($$renderer) {}";
    assert_eq!(render(&ast), expected);
}

/// Mirror of `packages/svelte/tests/snapshot/samples/hello-world/_expected/server/index.svelte.js`.
#[test]
fn snapshot_hello_world_server() {
    let ast = json!({
        "type": "Program",
        "body": [
            {
                "type": "ImportDeclaration",
                "specifiers": [{
                    "type": "ImportNamespaceSpecifier",
                    "local": { "type": "Identifier", "name": "$" }
                }],
                "source": { "type": "Literal", "raw": "'svelte/internal/server'" }
            },
            {
                "type": "ExportDefaultDeclaration",
                "declaration": {
                    "type": "FunctionDeclaration",
                    "async": false,
                    "generator": false,
                    "id": { "type": "Identifier", "name": "Hello_world" },
                    "params": [{ "type": "Identifier", "name": "$$renderer" }],
                    "body": {
                        "type": "BlockStatement",
                        "body": [{
                            "type": "ExpressionStatement",
                            "expression": {
                                "type": "CallExpression",
                                "callee": {
                                    "type": "MemberExpression",
                                    "object": { "type": "Identifier", "name": "$$renderer" },
                                    "property": { "type": "Identifier", "name": "push" },
                                    "computed": false
                                },
                                "arguments": [{
                                    "type": "TemplateLiteral",
                                    "quasis": [{ "value": { "raw": "<h1>hello world</h1>" } }],
                                    "expressions": []
                                }]
                            }
                        }]
                    }
                }
            }
        ]
    });
    let expected = "import * as $ from 'svelte/internal/server';\n\nexport default function Hello_world($$renderer) {\n\t$$renderer.push(`<h1>hello world</h1>`);\n}";
    assert_eq!(render(&ast), expected);
}

#[test]
fn class_with_method() {
    let ast = json!({
        "type": "ClassDeclaration",
        "id": { "type": "Identifier", "name": "Foo" },
        "body": {
            "type": "ClassBody",
            "body": [{
                "type": "MethodDefinition",
                "kind": "method",
                "static": false,
                "computed": false,
                "key": { "type": "Identifier", "name": "bar" },
                "value": {
                    "type": "FunctionExpression",
                    "async": false,
                    "generator": false,
                    "params": [],
                    "body": {
                        "type": "BlockStatement",
                        "body": [{
                            "type": "ReturnStatement",
                            "argument": { "type": "Literal", "raw": "1" }
                        }]
                    }
                }
            }]
        }
    });
    let expected = "class Foo {\n\tbar() {\n\t\treturn 1;\n\t}\n}";
    assert_eq!(render(&ast), expected);
}
