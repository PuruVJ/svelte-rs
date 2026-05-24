#!/usr/bin/env node
// Run bench suite and append summary to BENCHMARK_BASELINE.md

import { spawnSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const pkgRoot = resolve(here, '..');
const baselinePath = resolve(pkgRoot, 'BENCHMARK_BASELINE.md');

const FIXTURES = [
	'hello-world',
	'skip-static-subtree',
	'props-identifier',
	'function-prop-no-getter',
];

function run(cmd, args) {
	const r = spawnSync(cmd, args, { encoding: 'utf8', cwd: pkgRoot });
	if (r.status !== 0) {
		console.error(r.stderr || r.stdout);
		throw new Error(`${cmd} ${args.join(' ')} failed`);
	}
	return r.stdout;
}

const lines = [
	'# Compile benchmark baseline (svelte-compiler-perf)',
	'',
	`Generated: ${new Date().toISOString()}`,
	'',
	'## End-to-end (sandbox vs upstream, client, 1000 iter)',
	'',
];

for (const mode of ['client', 'server']) {
	lines.push(`### ${mode}`);
	lines.push('');
	lines.push('```');
	lines.push(
		run('node', ['bench/bench.mjs', '--iter', '1000', '--mode', mode]).trim()
	);
	lines.push('```');
	lines.push('');
}

lines.push('## Per-phase (sandbox, 2000 iter)');
lines.push('');

for (const name of FIXTURES) {
	const rel = `packages/svelte/tests/snapshot/samples/${name}/index.svelte`;
	for (const mode of ['client', 'server']) {
		lines.push(`### ${name} (${mode})`);
		lines.push('');
		lines.push('```');
		lines.push(
			run('node', [
				'bench/bench-phases.mjs',
				'--fixture',
				rel,
				'--mode',
				mode,
				'--iter',
				'2000',
			]).trim()
		);
		lines.push('```');
		lines.push('');
	}
}

writeFileSync(baselinePath, lines.join('\n'));
console.log(`Wrote ${baselinePath}`);
