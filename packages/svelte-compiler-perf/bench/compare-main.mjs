#!/usr/bin/env node
// Compare current compiler vs main branch baseline.

import { readFileSync, writeFileSync, mkdtempSync, rmSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { execSync } from 'node:child_process';
import { tmpdir } from 'node:os';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, '../../..');
const fixtures = JSON.parse(readFileSync(resolve(here, 'fixtures.json'), 'utf8'));
const iter = +process.argv.find((_, i, a) => a[i - 1] === '--iter') || 2000;

const tmp = mkdtempSync(resolve(tmpdir(), 'svelte-main-compiler-'));
try {
	execSync(`git archive main packages/svelte/src/compiler packages/svelte/src/utils.js packages/svelte/src/escaping.js packages/svelte/src/constants.js packages/svelte/src/html-tree-validation.js packages/svelte/src/version.js packages/svelte/src/internal | tar -x -C "${tmp}"`, {
		cwd: repoRoot,
		stdio: 'pipe'
	});

	const mainCompiler = resolve(tmp, 'packages/svelte/src/compiler/index.js');
	const curCompiler = resolve(repoRoot, 'packages/svelte/src/compiler/index.js');

	const { compile: mainCompile } = await import(pathToFileURL(mainCompiler).href);
	const { compile: curCompile } = await import(pathToFileURL(curCompiler).href);

	function time(name, compile, rel) {
		const source = readFileSync(resolve(repoRoot, rel), 'utf8');
		const opts = { generate: 'client', filename: rel };
		const fn = () => compile(source, opts);
		for (let i = 0; i < 50; i++) fn();
		const t0 = process.hrtime.bigint();
		for (let i = 0; i < iter; i++) fn();
		const ms = Number(process.hrtime.bigint() - t0) / 1e6 / iter;
		return { name, ms };
	}

	console.log(`iter=${iter} mode=client\n`);
	console.log('fixture                      | main ms | cur ms | ratio');
	console.log('-----------------------------|---------|--------|------');

	let mainSum = 0;
	let curSum = 0;
	for (const rel of fixtures) {
		const main = time('main', mainCompile, rel);
		const cur = time('cur', curCompile, rel);
		const ratio = cur.ms / main.ms;
		mainSum += main.ms;
		curSum += cur.ms;
		const label = rel.split('/').slice(-2, -1)[0].padEnd(28);
		console.log(`${label} | ${main.ms.toFixed(4).padStart(7)} | ${cur.ms.toFixed(4).padStart(6)} | ${ratio.toFixed(2)}x`);
	}

	console.log(`\nMean: main=${(mainSum / fixtures.length).toFixed(4)} cur=${(curSum / fixtures.length).toFixed(4)} ratio=${(curSum / mainSum).toFixed(2)}x`);
} finally {
	rmSync(tmp, { recursive: true, force: true });
}
