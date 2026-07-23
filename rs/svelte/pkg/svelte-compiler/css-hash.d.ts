/** @param {string} str */
export function svelteHash(str: string): string;

/** @param {string} filename */
export function getComponentName(filename: string): string;

/** @param {string} name */
export function sanitizeExportName(name: string): string;

/** @param {string} source */
export function extractCssStyles(source: string): string;

export function defaultCssHash(args: {
	css: string;
	filename: string;
	name?: string;
	hash?: (str: string) => string;
}): string;

export function resolveCssHashOption(
	source: string,
	options: import('svelte/compiler').CompileOptions
): string | undefined;
