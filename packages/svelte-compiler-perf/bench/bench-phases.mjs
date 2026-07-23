#!/usr/bin/env node
// Per-phase compile benchmark (parse / analyze / transform / codegen / e2e).
//
// Usage:
//   node bench/bench-phases.mjs [--fixture path] [--mode client|server] [--phase all|parse|analyze|transform|codegen|e2e] [--iter N] [--profile]

import { readFileSync } from 'node:fs';
import { resolve, dirname, basename } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { print } from 'esrap';
import ts from 'esrap/languages/ts';

const here = dirname(fileURLToPath(import.meta.url));
const pkgRoot = resolve(here, '..');
const repoRoot = resolve(pkgRoot, '../..');

const DEFAULT_FIXTURE =
	'packages/svelte/tests/snapshot/samples/skip-static-subtree/index.svelte';

const args = process.argv.slice(2);
let iter = 2000;
let mode = 'client';
let phase = 'all';
let fixtureRel = DEFAULT_FIXTURE;
let profile = false;

for (let i = 0; i < args.length; i++) {
	const a = args[i];
	if (a === '--iter' || a === '--iterations') iter = +args[++i] || 2000;
	else if (a === '--mode') mode = args[++i] || 'client';
	else if (a === '--phase') phase = args[++i] || 'all';
	else if (a === '--fixture') fixtureRel = args[++i];
	else if (a === '--profile') profile = true;
	else if (a === '-h' || a === '--help') {
		console.log(
			'node bench/bench-phases.mjs [--fixture path] [--mode client|server] [--phase all|parse|analyze|transform|codegen|e2e] [--iter N] [--profile]'
		);
		process.exit(0);
	}
}

const abs = resolve(repoRoot, fixtureRel);
const source = readFileSync(abs, 'utf8');
const filename = abs;

const compilerUrl = pathToFileURL(resolve(pkgRoot, 'src/index.js')).href;
const parseUrl = pathToFileURL(resolve(pkgRoot, 'src/phases/1-parse/index.js')).href;
const analyzeUrl = pathToFileURL(resolve(pkgRoot, 'src/phases/2-analyze/index.js')).href;
const validateUrl = pathToFileURL(resolve(pkgRoot, 'src/validate-options.js')).href;
const stateUrl = pathToFileURL(resolve(pkgRoot, 'src/state.js')).href;
const transformIndexUrl = pathToFileURL(resolve(pkgRoot, 'src/phases/3-transform/index.js')).href;
const clientUrl = pathToFileURL(
	resolve(pkgRoot, 'src/phases/3-transform/client/transform-client.js')
).href;
const serverUrl = pathToFileURL(
	resolve(pkgRoot, 'src/phases/3-transform/server/transform-server.js')
).href;
const tsStripUrl = pathToFileURL(
	resolve(pkgRoot, 'src/phases/1-parse/remove_typescript_nodes.js')
).href;

const { compile } = await import(compilerUrl);
const { parse: parseSource } = await import(parseUrl);
const { analyze_component } = await import(analyzeUrl);
const { validate_component_options } = await import(validateUrl);
const state = await import(stateUrl);
const { remove_typescript_nodes } = await import(tsStripUrl);
const { client_component } = await import(clientUrl);
const { server_component } = await import(serverUrl);

function buildOptions() {
	state.reset({ warning: () => true, filename });
	const validated = validate_component_options({ generate: mode, filename }, '');
	return validated;
}

function prepareParsed() {
	let parsed = parseSource(source);
	const { customElement: _ce, ...parsed_options } = parsed.options || {};
	const combined = {
		...buildOptions(),
		...parsed_options,
		css: 'css' in parsed_options ? () => parsed_options.css ?? 'external' : () => 'external',
		runes: 'runes' in parsed_options ? () => parsed_options.runes : () => undefined
	};
	if (parsed.metadata.ts) {
		parsed = {
			...parsed,
			fragment: parsed.fragment && remove_typescript_nodes(parsed.fragment),
			instance: parsed.instance && remove_typescript_nodes(parsed.instance),
			module: parsed.module && remove_typescript_nodes(parsed.module)
		};
	}
	return { parsed, combined };
}

function prepareAnalysis() {
	const { parsed, combined } = prepareParsed();
	const analysis = analyze_component(parsed, source, combined);
	return { analysis, combined };
}

function runTransform(analysis, combined) {
	state.reset({ warning: () => true, filename });
	return combined.generate === 'server'
		? server_component(analysis, combined)
		: client_component(analysis, combined);
}

function timePhase(name, fn, iters) {
	const warm = Math.min(50, Math.max(1, Math.floor(iters / 20)));
	for (let i = 0; i < warm; i++) fn();
	const started = process.hrtime.bigint();
	for (let i = 0; i < iters; i++) fn();
	const elapsed = Number(process.hrtime.bigint() - started) / 1e6;
	return { phase: name, iters, totalMs: elapsed, perMs: elapsed / iters };
}

const phases = phase === 'all' ? ['parse', 'analyze', 'transform', 'codegen', 'e2e'] : [phase];
const results = [];

for (const p of phases) {
	switch (p) {
		case 'parse':
			results.push(timePhase('parse', () => parseSource(source), iter));
			break;
		case 'analyze': {
			const { parsed, combined } = prepareParsed();
			results.push(
				timePhase('analyze', () => analyze_component(parsed, source, combined), iter)
			);
			break;
		}
		case 'transform':
			results.push(
				timePhase('transform', () => {
					const { analysis, combined } = prepareAnalysis();
					runTransform(analysis, combined);
				}, iter)
			);
			break;
		case 'codegen':
			results.push(
				timePhase('codegen', () => {
					const { analysis, combined } = prepareAnalysis();
					const program = runTransform(analysis, combined);
					print(/** @type {any} */ (program), ts({ comments: analysis.comments }), {
						sourceMapContent: source,
						sourceMapSource: filename
					});
				}, iter)
			);
			break;
		case 'e2e':
			results.push(
				timePhase('e2e', () => compile(source, { generate: mode, filename }), iter)
			);
			break;
		default:
			console.error(`unknown phase: ${p}`);
			process.exit(2);
	}
}

console.log(`fixture: ${fixtureRel} (${basename(fixtureRel)}) mode=${mode} iter=${iter}`);
console.log('phase     | ms/iter');
console.log('----------|--------');
for (const r of results) {
	console.log(`${r.phase.padEnd(9)} | ${r.perMs.toFixed(4)}`);
}

if (profile) {
	console.log('\n(profile mode: re-run with node --cpu-prof bench/bench-phases.mjs ...)');
}
