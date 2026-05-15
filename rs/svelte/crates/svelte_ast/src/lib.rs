//! Svelte template AST.
//!
//! Ported from:
//! - `packages/svelte/src/compiler/types/template.d.ts` (type declarations)
//! - `packages/svelte/src/compiler/phases/nodes.js`     (node constructor calls)
//! - `packages/svelte/src/compiler/phases/1-parse/index.js` (live shape of
//!   the parser output — see Root construction at lines 106-120)
//! - `packages/svelte/src/compiler/index.js::to_public_ast` (the cleaner that
//!   strips `metadata` fields before `parse(..., { modern: true })` returns)
//!
//! Wire-format invariant: this crate's `Serialize` impls produce JSON that
//! matches what `JSON.stringify(parse(source, { modern: true }))` emits in the
//! upstream Svelte 5.55.7 compiler.
//!
//! Every node struct carries its own `type` discriminator field. The
//! container enums (`FragmentChild`, `ElementAttribute`, `AttributeValuePart`)
//! are `#[serde(untagged)]` because their variants are uniquely identified by
//! that struct-level discriminator. This mirrors the JS data model (every
//! node has `type` on itself) and lets the same struct be used in both
//! tagged-enum and "naked" `Vec` contexts without losing the type.
//!
//! ESTree sub-trees (`Expression`, `Pattern`, `Program`, etc.) are modelled as
//! `serde_json::Value` for now. The OXC adapter in `svelte_parse` will fill
//! them with estree-shaped JSON.

#![forbid(unsafe_code)]

pub mod attributes;
pub mod blocks;
pub mod css;
pub mod elements;
pub mod fragment;
pub mod position;
pub mod root;
pub mod tags;

pub use attributes::*;
pub use blocks::*;
// Don't glob-export `css::*` — it has its own type names that clash with
// the top-level template `Block`, `Comment`, etc. Use `svelte_ast::css::Foo`.
pub use elements::*;
pub use fragment::*;
pub use position::*;
pub use root::*;
pub use tags::*;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn roundtrip_canary_top_level_shape() {
        let opaque_program = json!({ "type": "Program" });

        let root = Root {
            css: None,
            js: vec![],
            start: 0,
            end: 76,
            kind: RootKind::Root,
            fragment: Fragment {
                kind: FragmentKind::Fragment,
                nodes: vec![
                    FragmentChild::Comment(Comment {
                        kind: CommentKind::Comment,
                        start: 0,
                        end: 27,
                        data: "should not error out".to_string(),
                    }),
                    FragmentChild::Text(Text {
                        kind: TextKind::Text,
                        start: 27,
                        end: 28,
                        raw: "\n".to_string(),
                        data: "\n".to_string(),
                    }),
                ],
            },
            options: None,
            comments: vec![],
            instance: Some(Script {
                kind: ScriptKind::Script,
                start: 28,
                end: 76,
                context: ScriptContext::Default,
                content: opaque_program.clone(),
                attributes: vec![Attribute {
                    kind: AttributeKind::Attribute,
                    start: 36,
                    end: 45,
                    name: "lang".to_string(),
                    name_loc: Some(SourceLocation {
                        start: Position {
                            line: 2,
                            column: 8,
                            character: Some(36),
                        },
                        end: Position {
                            line: 2,
                            column: 12,
                            character: Some(40),
                        },
                    }),
                    value: AttributeValue::Many(vec![AttributeValuePart::Text(Text {
                        kind: TextKind::Text,
                        start: 42,
                        end: 44,
                        raw: "ts".to_string(),
                        data: "ts".to_string(),
                    })]),
                }],
            }),
            module: None,
        };

        let serialized = serde_json::to_value(&root).unwrap();

        assert_eq!(serialized["type"], "Root");
        assert_eq!(serialized["start"], 0);
        assert_eq!(serialized["end"], 76);
        assert_eq!(serialized["css"], Value::Null);
        assert_eq!(serialized["js"], json!([]));
        assert_eq!(serialized["options"], Value::Null);
        assert_eq!(serialized["comments"], json!([]));
        assert_eq!(serialized["fragment"]["type"], "Fragment");

        let nodes = serialized["fragment"]["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0]["type"], "Comment");
        assert_eq!(nodes[0]["data"], "should not error out");
        assert_eq!(nodes[1]["type"], "Text");
        assert_eq!(nodes[1]["raw"], "\n");

        assert_eq!(serialized["instance"]["type"], "Script");
        assert_eq!(serialized["instance"]["context"], "default");
        assert_eq!(serialized["instance"]["attributes"][0]["type"], "Attribute");
        assert_eq!(serialized["instance"]["attributes"][0]["name"], "lang");
        assert_eq!(
            serialized["instance"]["attributes"][0]["name_loc"]["start"]["character"],
            36
        );
    }

    #[test]
    fn script_context_lowercase() {
        let s = Script {
            kind: ScriptKind::Script,
            start: 0,
            end: 0,
            context: ScriptContext::Module,
            content: Value::Null,
            attributes: vec![],
        };
        let j = serde_json::to_value(&s).unwrap();
        assert_eq!(j["context"], "module");
    }

    #[test]
    fn empty_root_shape() {
        let root = Root {
            css: None,
            js: vec![],
            start: 0,
            end: 0,
            kind: RootKind::Root,
            fragment: Fragment::empty(),
            options: None,
            comments: vec![],
            instance: None,
            module: None,
        };
        let j = serde_json::to_value(&root).unwrap();
        assert_eq!(j["type"], "Root");
        assert!(!j.as_object().unwrap().contains_key("instance"));
        assert!(!j.as_object().unwrap().contains_key("module"));
    }

    #[test]
    fn svelte_body_shape() {
        let el = SvelteBody {
            kind: SvelteBodyKind::SvelteBody,
            start: 0,
            end: 10,
            name: SvelteBodyName::Value,
            name_loc: SourceLocation {
                start: Position {
                    line: 1,
                    column: 0,
                    character: None,
                },
                end: Position {
                    line: 1,
                    column: 11,
                    character: None,
                },
            },
            attributes: vec![],
            fragment: Fragment::empty(),
        };
        let child = FragmentChild::SvelteBody(el);
        let j = serde_json::to_value(&child).unwrap();
        assert_eq!(j["type"], "SvelteBody");
        assert_eq!(j["name"], "svelte:body");
    }

    #[test]
    fn if_block_shape() {
        let b = IfBlock {
            kind: IfBlockKind::IfBlock,
            start: 0,
            end: 30,
            elseif: false,
            test: json!({ "type": "Identifier", "name": "x" }),
            consequent: Fragment::empty(),
            alternate: None,
        };
        let child = FragmentChild::IfBlock(b);
        let j = serde_json::to_value(&child).unwrap();
        assert_eq!(j["type"], "IfBlock");
        assert_eq!(j["elseif"], false);
        assert_eq!(j["test"]["name"], "x");
    }

    #[test]
    fn await_block_catch_field_name() {
        let b = AwaitBlock {
            kind: AwaitBlockKind::AwaitBlock,
            start: 0,
            end: 30,
            expression: json!(null),
            value: None,
            error: None,
            pending: None,
            then: None,
            catch_: None,
        };
        let child = FragmentChild::AwaitBlock(b);
        let j = serde_json::to_value(&child).unwrap();
        assert_eq!(j["type"], "AwaitBlock");
        assert!(j.as_object().unwrap().contains_key("catch"));
        assert!(!j.as_object().unwrap().contains_key("catch_"));
    }

    #[test]
    fn attribute_value_empty_serializes_as_true() {
        let a = Attribute {
            kind: AttributeKind::Attribute,
            start: 0,
            end: 8,
            name: "disabled".to_string(),
            name_loc: None,
            value: AttributeValue::Empty(true),
        };
        let j = serde_json::to_value(&a).unwrap();
        assert_eq!(j["value"], Value::Bool(true));
    }

    /// `AttributeValue::Single(ExpressionTag)` must serialize as an object that
    /// includes `type: "ExpressionTag"` — this is the case that initially
    /// pushed us to put `type` discriminators directly on every struct.
    #[test]
    fn attribute_value_single_includes_type_discriminator() {
        let a = Attribute {
            kind: AttributeKind::Attribute,
            start: 0,
            end: 10,
            name: "runes".to_string(),
            name_loc: None,
            value: AttributeValue::Single(ExpressionTag {
                kind: ExpressionTagKind::ExpressionTag,
                start: 6,
                end: 12,
                expression: json!({ "type": "Literal", "value": true }),
            }),
        };
        let j = serde_json::to_value(&a).unwrap();
        assert_eq!(j["value"]["type"], "ExpressionTag");
    }
}
