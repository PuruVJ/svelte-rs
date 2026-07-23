/**
 * Mirrors `packages/svelte/src/utils.js` `hash()` and default `cssHash` option.
 */

/**
 * @param {string} str
 * @returns {string}
 */
export function svelteHash(str) {
	str = str.replace(/\r/g, '');
	let hash = 5381;
	let i = str.length;
	while (i--) hash = ((hash << 5) - hash) ^ str.charCodeAt(i);
	return (hash >>> 0).toString(36);
}

/**
 * @param {string} filename
 */
export function getComponentName(filename) {
	const parts = filename.split(/[/\\]/);
	const basename = parts.pop() ?? '';
	const lastDir = parts.at(-1);
	let name = basename.replace(/\.svelte$/, '');
	if (name === 'index' && lastDir && lastDir !== 'src') {
		name = lastDir;
	}
	return name ? name[0].toUpperCase() + name.slice(1) : 'Component';
}

/** @param {string} name */
export function sanitizeExportName(name) {
	let s = name.replace(/[^a-zA-Z0-9_$]/g, '_');
	if (/^[0-9]/.test(s)) s = '_' + s;
	return s || 'Component';
}

/**
 * @param {string} source
 */
export function extractCssStyles(source) {
	const m = source.match(/<style(?:\s[^>]*)?>([\s\S]*?)<\/style>/i);
	return m ? m[1] : '';
}

/**
 * Default `cssHash` — upstream `validate-options.js`.
 * @param {{ css: string, filename: string, name?: string, hash?: typeof svelteHash }} args
 */
export function defaultCssHash({ css, filename, name, hash = svelteHash }) {
	const basis = filename === '(unknown)' ? css : (filename ?? css);
	return `svelte-${hash(basis)}`;
}

/**
 * Call a user `cssHash` function with the same args upstream passes in analyze.
 * @param {string} source
 * @param {import('svelte/compiler').CompileOptions} options
 */
export function resolveCssHashOption(source, options) {
	const css = extractCssStyles(source);
	const filename = options.filename ?? '(unknown)';
	const rawName = options.name ?? getComponentName(filename);
	const name = sanitizeExportName(rawName);
	const cssHash = options.cssHash;
	if (typeof cssHash === 'function') {
		return cssHash({ hash: svelteHash, css, name, filename });
	}
	if (typeof cssHash === 'string') {
		return cssHash;
	}
	if (css) {
		return defaultCssHash({ css, filename, name, hash: svelteHash });
	}
	return undefined;
}
