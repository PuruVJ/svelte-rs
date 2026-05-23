/**
 * JS-shaped facade over `svelte_wasm` — mirrors `packages/svelte/src/compiler/index.js`.
 *
 * Function-valued options (`customElement`, `css`, `cssHash`) are resolved on the
 * JS side before calling WASM. `cssHash` receives the same `{ hash, css, name,
 * filename }` object as upstream analyze.
 */

import * as wasm from '../../crates/svelte_wasm/pkg/svelte_wasm.js';
import { resolveCssHashOption } from './css-hash.js';

/**
 * @param {string} source
 * @param {import('svelte/compiler').CompileOptions | import('svelte/compiler').ModuleCompileOptions} [options]
 */
export function normalizeCompileOptions(source, options = {}) {
	const filename = options.filename ?? '(unknown)';
	const out = { ...options };
	delete out.warningFilter;
	if (typeof options.customElement === 'function') {
		out.customElement = options.customElement({ filename });
	}
	if (typeof options.css === 'function') {
		out.css = options.css({ filename });
	}
	const resolvedCssHash = resolveCssHashOption(source, options);
	if (resolvedCssHash !== undefined) {
		out.cssHash = resolvedCssHash;
	} else {
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
		wasm.compile(source, normalizeCompileOptions(source, options))
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

export { resolveCssHashOption, svelteHash, defaultCssHash, extractCssStyles } from './css-hash.js';

/**
 * @param {string} source
 * @param {Array<{ code: string, map?: string, dependencies?: string[] }>} processed
 */
export async function preprocessCombine(source, processed) {
	return wasm.preprocess_combine(source, processed);
}
