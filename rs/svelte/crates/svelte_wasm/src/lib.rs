//! wasm-bindgen surface.
//!
//! Exposes `compile`, `parse`, `compile_module`, `preprocess`, `migrate` to JS
//! with return values shaped identically to those from
//! `packages/svelte/src/compiler/index.js`.

#![forbid(unsafe_code)]

use wasm_bindgen::prelude::*;

/// `parse(source, options?)` — returns the parsed Svelte AST as a plain JS
/// object.
#[wasm_bindgen]
pub fn parse(source: &str, options: JsValue) -> Result<JsValue, JsValue> {
    let parse_options: svelte_compiler::ParseOptions = if options.is_undefined() || options.is_null()
    {
        svelte_compiler::ParseOptions::default()
    } else {
        serde_wasm_bindgen::from_value(options).map_err(|e| JsValue::from_str(&e.to_string()))?
    };
    // STUB: typed Root no longer serializes; wasm parse() returns null
    // until Phase D ports a typed -> estree wire-format walker.
    let _root = svelte_compiler::parse(source, parse_options)
        .map_err(|e| JsValue::from_str(&format!("{e:?}")))?;
    Ok(JsValue::NULL)
}

/// `preprocess(source, processed)` — combine the outputs of already-invoked
/// preprocessor functions (called by the JS side) into a single result.
/// The JS bridge passes an array of `{ code, map?, dependencies? }` objects.
#[wasm_bindgen]
pub fn preprocess_combine(source: &str, processed: JsValue) -> Result<JsValue, JsValue> {
    let processed_vec: Vec<JsProcessed> =
        serde_wasm_bindgen::from_value(processed).map_err(|e| JsValue::from_str(&e.to_string()))?;
    let rust_processed: Vec<svelte_preprocess::Processed> = processed_vec
        .into_iter()
        .map(|p| svelte_preprocess::Processed {
            code: p.code,
            map: p.map,
            dependencies: p.dependencies.unwrap_or_default(),
        })
        .collect();
    let result = svelte_preprocess::combine(source, rust_processed);

    let obj = js_sys::Object::new();
    js_sys::Reflect::set(
        &obj,
        &JsValue::from_str("code"),
        &JsValue::from_str(&result.code),
    )?;
    let deps = js_sys::Array::new();
    for d in &result.dependencies {
        deps.push(&JsValue::from_str(d));
    }
    js_sys::Reflect::set(&obj, &JsValue::from_str("dependencies"), &deps)?;
    if let Some(m) = result.map {
        js_sys::Reflect::set(&obj, &JsValue::from_str("map"), &JsValue::from_str(&m))?;
    }
    Ok(obj.into())
}

#[derive(serde::Deserialize)]
struct JsProcessed {
    code: String,
    #[serde(default)]
    map: Option<String>,
    #[serde(default)]
    dependencies: Option<Vec<String>>,
}

/// `compile(source, options)` — full pipeline. Returns `{ js, warnings }` for
/// now; will grow `css`, `ast`, `stats` as those land.
#[wasm_bindgen]
pub fn compile(source: &str, component_name: &str, options: JsValue) -> Result<JsValue, JsValue> {
    let compile_options: svelte_compiler::CompileOptions =
        if options.is_undefined() || options.is_null() {
            svelte_compiler::CompileOptions::default()
        } else {
            serde_wasm_bindgen::from_value(options).map_err(|e| JsValue::from_str(&e.to_string()))?
        };
    let result = svelte_compiler::compile(source, component_name, compile_options)
        .map_err(|e| JsValue::from_str(&format!("{e:?}")))?;

    let obj = js_sys::Object::new();
    js_sys::Reflect::set(
        &obj,
        &JsValue::from_str("js"),
        &JsValue::from_str(&result.js),
    )?;
    js_sys::Reflect::set(
        &obj,
        &JsValue::from_str("warnings"),
        &js_sys::Array::new().into(),
    )?;
    Ok(obj.into())
}
