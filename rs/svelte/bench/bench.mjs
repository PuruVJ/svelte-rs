#!/usr/bin/env node
// Benchmark JS svelte vs Rust WASM vs Rust native compile across a fixture
// set. Runs in-process for JS and WASM; spawns `svelte-rs --bench` for the
// native CLI (the CLI loops internally so we exclude process-startup cost).
//
// Usage:
//   node bench/bench.mjs                 # default fixture set + 1000 iter
//   node bench/bench.mjs --iter 5000     # set iteration count
//   node bench/bench.mjs --fixture path  # add a single fixture by path
//   node bench/bench.mjs --json          # emit JSON (for tooling)
//
// Output columns:
//   fixture | LOC | mode | ms/iter | factor
//
// `factor` is relative to the JS baseline of the same fixture (JS = 1.00x).

import { readFileSync, statSync } from 'node:fs';
import { resolve, dirname, basename } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { spawnSync } from 'node:child_process';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, '../../..');

// Default fixtures — currently-passing samples across client + server.
// Override with one or more `--fixture PATH` flags to bench a different set.
const DEFAULT_FIXTURES = [
	'packages/svelte/tests/snapshot/samples/hello-world/index.svelte',
	'packages/svelte/tests/snapshot/samples/imports-in-modules/index.svelte',
	'packages/svelte/tests/snapshot/samples/bind-this/index.svelte',
	'packages/svelte/tests/snapshot/samples/purity/index.svelte',
	'packages/svelte/tests/snapshot/samples/text-nodes-deriveds/index.svelte',
	'packages/svelte/tests/snapshot/samples/functional-templating/index.svelte',
	'packages/svelte/tests/snapshot/samples/state-proxy-literal/index.svelte',
	'packages/svelte/tests/snapshot/samples/nullish-coallescence-omittance/index.svelte',
	'packages/svelte/tests/snapshot/samples/each-index-non-null/index.svelte',
	'packages/svelte/tests/snapshot/samples/each-string-template/index.svelte',
	'packages/svelte/tests/snapshot/samples/delegated-locally-declared-shadowed/index.svelte',
	'packages/svelte/tests/snapshot/samples/svelte-element/index.svelte',
	'packages/svelte/tests/snapshot/samples/class-state-field-constructor-assignment/index.svelte',
	'packages/svelte/tests/snapshot/samples/function-prop-no-getter/index.svelte',
	'packages/svelte/tests/snapshot/samples/props-identifier/index.svelte',
	'packages/svelte/tests/snapshot/samples/skip-static-subtree/index.svelte',
];

// CLI args -----------------------------------------------------------------

const args = process.argv.slice(2);
let iter = 1000;
let asJson = false;
let check = false;
let mode = 'client';
const extra = [];
for (let i = 0; i < args.length; i++) {
	const a = args[i];
	if (a === '--iter' || a === '--iterations') iter = +args[++i] || 1000;
	else if (a === '--json') asJson = true;
	else if (a === '--check') check = true;
	else if (a === '--mode') mode = args[++i] || 'client';
	else if (a === '--fixture') extra.push(args[++i]);
	else if (a === '-h' || a === '--help') {
		console.log('node bench/bench.mjs [--iter N] [--mode client|server] [--fixture path] [--json] [--check]');
		console.log('  --mode   Which generate target to bench (default: client).');
		console.log('  --check  Compare WASM/native output against JS (byte-equal) and report.');
		process.exit(0);
	}
}

const fixtures = extra.length > 0 ? extra : DEFAULT_FIXTURES;

// Load compilers -----------------------------------------------------------

const jsCompilerPath = resolve(repoRoot, 'packages/svelte/src/compiler/index.js');
const wasmPkgPath = resolve(here, '../pkg/svelte-compiler/index.js');
const nativeBin = resolve(here, '../target/release/svelte-rs');

const { compile: jsCompile } = await import(pathToFileURL(jsCompilerPath).href);
const { compileSync: wasmCompile } = await import(pathToFileURL(wasmPkgPath).href);

// Sanity check the native binary is reachable.
try {
	statSync(nativeBin);
} catch (e) {
	console.error(`Native binary missing at ${nativeBin}`);
	console.error('Build it first: cargo build --release -p svelte_compiler --bin svelte-rs');
	process.exit(1);
}

// Timing helpers ------------------------------------------------------------

function timeFn(name, fn, iters) {
	// Warm-up: 50 iterations not included.
	const warm = Math.min(50, Math.max(1, Math.floor(iters / 20)));
	for (let i = 0; i < warm; i++) fn();
	const started = process.hrtime.bigint();
	for (let i = 0; i < iters; i++) fn();
	const elapsed = Number(process.hrtime.bigint() - started) / 1e6;
	return { name, iters, totalMs: elapsed, perMs: elapsed / iters };
}

function timeNative(fixturePath, iters) {
	const args = ['--bench', String(iters), fixturePath];
	if (mode === 'server') args.unshift('--ssr');
	const out = spawnSync(nativeBin, args, { encoding: 'utf8' });
	if (out.status !== 0) {
		return { name: 'native', iters, totalMs: NaN, perMs: NaN, err: out.stderr };
	}
	// stdout format: `iterations=… total_ms=… per_ms=…`
	const line = out.stdout.trim();
	const totalMs = +(line.match(/total_ms=([0-9.]+)/) || [])[1] || NaN;
	const perMs = +(line.match(/per_ms=([0-9.]+)/) || [])[1] || NaN;
	return { name: 'native', iters, totalMs, perMs };
}

// Run --------------------------------------------------------------------

const rows = [];
for (const rel of fixtures) {
	const abs = resolve(repoRoot, rel);
	let source;
	try {
		source = readFileSync(abs, 'utf8');
	} catch (e) {
		console.error(`skip ${rel} — ${e.message}`);
		continue;
	}
	const loc = source.split('\n').length;
	// Match svelte's filename → component-name convention: split on
	// non-identifier chars, PascalCase each part. So `index.svelte` →
	// `Index`, `hello-world.svelte` → `HelloWorld`.
	const stem = basename(abs).replace(/\.svelte$/, '');
	const name = stem
		.split(/[-_.]/)
		.filter(Boolean)
		.map((p) => p[0].toUpperCase() + p.slice(1))
		.join('') || 'Component';

	const compileOptions = { generate: mode, filename: abs };
	let js;
	try {
		js = timeFn('js', () => jsCompile(source, compileOptions), iter);
	} catch (e) {
		console.error(`JS compile of ${rel} threw: ${e.message}`);
		continue;
	}

	// WASM
	let wasmRow;
	try {
		wasmRow = timeFn('wasm', () => wasmCompile(source, compileOptions), iter);
	} catch (e) {
		wasmRow = { name: 'wasm', iters: iter, totalMs: NaN, perMs: NaN, err: e.message };
	}

	// Native CLI
	const nativeRow = timeNative(abs, iter);

	let checkResult;
	if (check) {
		try {
			const jsOut = jsCompile(source, compileOptions).js.code;
			const wasmOut = wasmCompile(source, compileOptions).js.code;
			const nativeArgs = [abs];
			if (mode === 'server') nativeArgs.unshift('--ssr');
			const nativeProc = spawnSync(nativeBin, nativeArgs, { encoding: 'utf8' });
			const nativeOut = nativeProc.stdout || '';
			checkResult = {
				wasmEq: wasmOut === jsOut,
				nativeEq: nativeOut === jsOut,
				wasmVsNative: wasmOut === nativeOut,
				jsLen: jsOut.length,
				wasmLen: wasmOut.length,
				nativeLen: nativeOut.length,
			};
		} catch (e) {
			checkResult = { err: e.message };
		}
	}

	rows.push({ fixture: rel, loc, js, wasm: wasmRow, native: nativeRow, check: checkResult });
}

// Output ------------------------------------------------------------------

if (asJson) {
	console.log(JSON.stringify(rows, null, 2));
	process.exit(0);
}

const colF = 60;
const header = ['fixture'.padEnd(colF), 'LOC', 'mode', 'ms/iter', 'factor'];
console.log(header.map((h, i) => i === 0 ? h : h.padStart(8)).join(' | '));
console.log('-'.repeat(colF + 8 + 8 + 10 + 10));

for (const row of rows) {
	const base = row.js.perMs;
	const print = (mode, r) => {
		const ms = r.perMs;
		const factor = isFinite(ms) ? (ms / base).toFixed(2) + 'x' : '—';
		const cells = [
			row.fixture.padEnd(colF),
			String(row.loc).padStart(8),
			mode.padStart(8),
			(isFinite(ms) ? ms.toFixed(4) : 'ERR').padStart(10),
			factor.padStart(10),
		];
		console.log(cells.join(' | '));
		if (r.err) console.log('     err:', r.err.slice(0, 200));
	};
	print('js', row.js);
	print('wasm', row.wasm);
	print('native', row.native);
	if (row.check) {
		const c = row.check;
		if (c.err) {
			console.log(`     check ERR: ${c.err}`);
		} else {
			const mark = (b) => (b ? 'OK ' : 'NEQ');
			console.log(
				`     check: wasm=${mark(c.wasmEq)} native=${mark(c.nativeEq)} ` +
					`(js=${c.jsLen}B, wasm=${c.wasmLen}B, native=${c.nativeLen}B)`
			);
		}
	}
}

// Summary -------------------------------------------------------------------

const meanFactor = (mode) => {
	const factors = rows
		.map((r) => r[mode].perMs / r.js.perMs)
		.filter((x) => isFinite(x));
	if (factors.length === 0) return NaN;
	return factors.reduce((a, b) => a + b, 0) / factors.length;
};

const fmtSize = (bytes) => {
	if (!isFinite(bytes)) return '—';
	if (bytes >= 1024 * 1024) return (bytes / 1024 / 1024).toFixed(2) + ' MiB';
	if (bytes >= 1024) return (bytes / 1024).toFixed(1) + ' KiB';
	return bytes + ' B';
};
const sizeOf = (p) => {
	try { return statSync(p).size; } catch { return NaN; }
};

console.log();
console.log(`Mean compile-time vs JS (lower = faster):`);
console.log(`  wasm   : ${meanFactor('wasm').toFixed(2)}x`);
console.log(`  native : ${meanFactor('native').toFixed(2)}x`);
console.log(`(iterations per fixture: ${iter})`);

console.log();
console.log(`Artifact sizes:`);
console.log(`  wasm bg     : ${fmtSize(sizeOf(resolve(here, '../crates/svelte_wasm/pkg/svelte_wasm_bg.wasm')))}`);
console.log(`  wasm glue   : ${fmtSize(sizeOf(resolve(here, '../crates/svelte_wasm/pkg/svelte_wasm.js')))}`);
console.log(`  native bin  : ${fmtSize(sizeOf(nativeBin))}`);
