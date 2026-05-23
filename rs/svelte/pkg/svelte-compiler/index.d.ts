import type {
	CompileOptions,
	CompileResult,
	ModuleCompileOptions,
	ParseOptions,
} from 'svelte/compiler';

export function normalizeCompileOptions(
	options?: CompileOptions | ModuleCompileOptions
): CompileOptions | ModuleCompileOptions;

export function compile(source: string, options?: CompileOptions): Promise<CompileResult>;

export function compileSync(source: string, options?: CompileOptions): CompileResult;

export function parse(source: string, options?: ParseOptions): Promise<unknown>;

export function initWasm(): Promise<void>;

export function preprocessCombine(
	source: string,
	processed: Array<{ code: string; map?: string; dependencies?: string[] }>
): Promise<{ code: string; map?: string; dependencies: string[] }>;

export { initWasm as init };
