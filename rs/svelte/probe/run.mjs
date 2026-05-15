// Probe script — invoked by `svelte_test_harness` (and by Rust integration tests).
//
// Runs the *existing JS compiler* (the golden master) on a fixture and prints
// the result as JSON to stdout. Anything the Rust port produces is diffed
// against this output.
//
// Usage:
//   node rs/svelte/probe/run.mjs <mode> <fixture-path>
//
// Modes:
//   parse        — parse(input, { modern: true, loose })
//   parse-legacy — parse(input)                              (legacy AST)
//   compile      — compile(input, { generate: 'client', ... })
//   compile-ssr  — compile(input, { generate: 'server', ... })
//
// <fixture-path> may be:
//   - a directory containing input.svelte (parser-modern / runtime-* style), OR
//   - a path to a .svelte file directly.
//
// The path is resolved against the current working directory, which the Rust
// harness sets to the repo root.

import { readFileSync, statSync, existsSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { compile, parse } from '../../../packages/svelte/src/compiler/index.js';

function die(msg, code = 2) {
	process.stderr.write(msg + '\n');
	process.exit(code);
}

function load_source(fixture_path) {
	const abs = resolve(fixture_path);
	if (!existsSync(abs)) die(`fixture not found: ${abs}`);
	const stat = statSync(abs);
	if (stat.isDirectory()) {
		const candidate = join(abs, 'input.svelte');
		if (existsSync(candidate)) return { path: candidate, source: readFileSync(candidate, 'utf8') };
		// runtime-* tests use main.svelte
		const main = join(abs, 'main.svelte');
		if (existsSync(main)) return { path: main, source: readFileSync(main, 'utf8') };
		die(`no input.svelte or main.svelte in ${abs}`);
	}
	return { path: abs, source: readFileSync(abs, 'utf8') };
}

function normalize_source(src) {
	// parser-modern/test.ts strips trailing whitespace + CR before parsing.
	return src.replace(/\s+$/, '').replace(/\r/g, '');
}

const [, , mode, fixture_arg] = process.argv;
if (!mode || !fixture_arg) die('usage: run.mjs <mode> <fixture-path>');

const { path, source: raw } = load_source(fixture_arg);
const source = normalize_source(raw);

const loose = fixture_arg.split('/').pop()?.startsWith('loose-') ?? false;

let result;
switch (mode) {
	case 'parse':
		result = parse(source, { modern: true, loose });
		break;
	case 'parse-legacy':
		result = parse(source);
		break;
	case 'compile':
		result = compile(source, { generate: 'client', filename: path });
		break;
	case 'compile-ssr':
		result = compile(source, { generate: 'server', filename: path });
		break;
	default:
		die(`unknown mode: ${mode}`);
}

// JSON.parse(JSON.stringify(...)) — same normalization the existing tests apply,
// so what we emit is byte-equivalent to what the test runner compares against.
process.stdout.write(JSON.stringify(JSON.parse(JSON.stringify(result)), null, '\t') + '\n');
