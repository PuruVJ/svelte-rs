/**
 * JS-shaped facade over `svelte_wasm` — mirrors `packages/svelte/src/compiler/index.js`.
 *
 * Only JSON-serializable options are forwarded to WASM (no `warningFilter` callbacks).
 * Function-valued options (`customElement`, `css`, `cssHash`) are resolved on the JS side
 * when possible before calling into Rust.
 */

import * as wasm from '../../crates/svelte_wasm/pkg/svelte_wasm.js';

/**
 * @param {import('svelte/compiler').CompileOptions | import('svelte/compiler').ModuleCompileOptions} options
 */
export function normalizeCompileOptions(options = {}) {
	const filename = options.filename ?? '(unknown)';
	const out = { ...options };
	delete out.warningFilter;
	if (typeof options.customElement === 'function') {
		out.customElement = options.customElement({ filename });
	}
	if (typeof options.css === 'function') {
		out.css = options.css({ filename });
	}
	if (typeof options.cssHash === 'function') {
		delete out.cssHash;
	}
	return out;
}

/**
 * @param {string} source
 * @param {import('svelte/compiler').CompileOptions} [options]
 * @returns {import('svelte/compiler').CompileResult}
 */
export function compileSync(source, options = {}) {
	return /** @type {import('svelte/compiler').CompileResult} */ (
		wasm.compile(source, normalizeCompileOptions(options))
	);
}

/**
 * @param {string} source
 * @param {import('svelte/compiler').CompileOptions} [options]
 * @returns {Promise<import('svelte/compiler').CompileResult>}
 */
export async function compile(source, options = {}) {
	return compileSync(source, options);
}

/**
 * @param {string} source
 * @param {import('svelte/compiler').ParseOptions} [options]
 */
export async function parse(source, options = {}) {
	return wasm.parse(source, options);
}

/** No-op for the nodejs wasm-pack target (WASM is loaded synchronously). */
export async function initWasm() {}

export { initWasm as init };

/**
 * @param {string} source
 * @param {Array<{ code: string, map?: string, dependencies?: string[] }>} processed
 */
export async function preprocessCombine(source, processed) {
	return wasm.preprocess_combine(source, processed);
}
