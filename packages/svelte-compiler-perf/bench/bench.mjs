#!/usr/bin/env node
// End-to-end compile benchmark for svelte-compiler-perf sandbox vs upstream JS compiler.
//
// Usage:
//   node bench/bench.mjs [--iter N] [--mode client|server] [--fixture path] [--json] [--check]

import { readFileSync, existsSync } from 'node:fs';
import { resolve, dirname, basename } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const pkgRoot = resolve(here, '..');
const repoRoot = resolve(pkgRoot, '../..');

const DEFAULT_FIXTURES = JSON.parse(
	readFileSync(resolve(here, 'fixtures.json'), 'utf8')
);

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
		console.log(
			'node bench/bench.mjs [--iter N] [--mode client|server] [--fixture path] [--json] [--check]'
		);
		process.exit(0);
	}
}

const fixtures = extra.length > 0 ? extra : DEFAULT_FIXTURES;

const sandboxPath = resolve(pkgRoot, 'src/index.js');
const upstreamPath = resolve(repoRoot, 'packages/svelte/src/compiler/index.js');

const { compile: sandboxCompile } = await import(pathToFileURL(sandboxPath).href);
const { compile: upstreamCompile } = await import(pathToFileURL(upstreamPath).href);

function timeFn(name, fn, iters) {
	const warm = Math.min(50, Math.max(1, Math.floor(iters / 20)));
	for (let i = 0; i < warm; i++) fn();
	const started = process.hrtime.bigint();
	for (let i = 0; i < iters; i++) fn();
	const elapsed = Number(process.hrtime.bigint() - started) / 1e6;
	return { name, iters, totalMs: elapsed, perMs: elapsed / iters };
}

function expectedPath(fixtureRel, generate) {
	const dir = dirname(fixtureRel);
	const base = basename(fixtureRel);
	const sub = generate === 'server' ? 'server' : 'client';
	return resolve(repoRoot, dir, `_expected/${sub}/${base}.js`);
}

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
	const compileOptions = { generate: mode, filename: abs };

	let sandbox;
	try {
		sandbox = timeFn('sandbox', () => sandboxCompile(source, compileOptions), iter);
	} catch (e) {
		console.error(`sandbox compile of ${rel} threw: ${e.message}`);
		continue;
	}

	let upstream;
	try {
		upstream = timeFn('upstream', () => upstreamCompile(source, compileOptions), iter);
	} catch (e) {
		upstream = { name: 'upstream', iters: iter, totalMs: NaN, perMs: NaN, err: e.message };
	}

	let checkResult;
	if (check) {
		try {
			const sandboxOut = sandboxCompile(source, compileOptions).js.code;
			const upstreamOut = upstreamCompile(source, compileOptions).js.code;
			const expPath = expectedPath(rel, mode);
			const expected = existsSync(expPath) ? readFileSync(expPath, 'utf8') : null;
			checkResult = {
				sandboxVsUpstream: sandboxOut === upstreamOut,
				sandboxVsExpected: expected ? sandboxOut === expected : null,
				upstreamVsExpected: expected ? upstreamOut === expected : null,
				sandboxLen: sandboxOut.length,
				upstreamLen: upstreamOut.length,
			};
		} catch (e) {
			checkResult = { err: e.message };
		}
	}

	rows.push({ fixture: rel, loc, sandbox, upstream, check: checkResult });
}

if (asJson) {
	console.log(JSON.stringify(rows, null, 2));
	process.exit(0);
}

const colF = 55;
console.log(['fixture'.padEnd(colF), 'LOC', 'compiler', 'ms/iter', 'vs upstream'].join(' | '));
console.log('-'.repeat(colF + 40));

for (const row of rows) {
	const base = row.upstream.perMs;
	const print = (label, r) => {
		const ms = r.perMs;
		const factor = isFinite(ms) && isFinite(base) ? (ms / base).toFixed(2) + 'x' : '—';
		console.log(
			[
				row.fixture.padEnd(colF),
				String(row.loc).padStart(8),
				label.padStart(10),
				(isFinite(ms) ? ms.toFixed(4) : 'ERR').padStart(10),
				factor.padStart(12),
			].join(' | ')
		);
		if (r.err) console.log('     err:', String(r.err).slice(0, 200));
	};
	print('sandbox', row.sandbox);
	print('upstream', row.upstream);
	if (row.check) {
		const c = row.check;
		if (c.err) console.log(`     check ERR: ${c.err}`);
		else {
			const mark = (b) => (b === null ? 'n/a' : b ? 'OK' : 'NEQ');
			console.log(
				`     check: sandbox=upstream ${mark(c.sandboxVsUpstream)} | ` +
					`sandbox=expected ${mark(c.sandboxVsExpected)} | upstream=expected ${mark(c.upstreamVsExpected)}`
			);
		}
	}
}

const meanFactor = () => {
	const factors = rows
		.map((r) => r.sandbox.perMs / r.upstream.perMs)
		.filter((x) => isFinite(x));
	return factors.length ? factors.reduce((a, b) => a + b, 0) / factors.length : NaN;
};

console.log();
console.log(`Mean sandbox/upstream: ${meanFactor().toFixed(3)}x (iterations=${iter}, mode=${mode})`);
