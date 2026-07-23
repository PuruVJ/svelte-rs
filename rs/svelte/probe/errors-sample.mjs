// Exercises a sample of diagnostic functions from packages/svelte/src/compiler/errors.js
// and prints the resulting message strings as JSON. The Rust differential test
// in `svelte_diagnostics/tests/diff_against_js.rs` calls its own generated
// equivalents and asserts byte equality against this output.
//
// Each entry is `[fn_name, [args]]`. The first arg passed to the JS function
// is the AST node — we use `null` for "no position" so we exercise the same
// code paths the Rust API exposes (where `span` is `Option<Span>`).
//
// To keep this dependency-light we import the JS module directly and catch
// the thrown InternalCompileError so we can read its `code`, `message`, and `position`.

import * as errors from '../../../packages/svelte/src/compiler/errors.js';
import * as warnings from '../../../packages/svelte/src/compiler/warnings.js';
import { warnings as warning_state } from '../../../packages/svelte/src/compiler/state.js';
import * as state from '../../../packages/svelte/src/compiler/state.js';

state.reset({});

const error_cases = [
	['options_invalid_value', ['foo']],
	['options_invalid_value', ['multi line\nvalue']],
	['options_removed', ['details']],
	['options_unrecognised', ['somekey']],
	['bind_invalid_name', ['mything']],
	['bind_invalid_name', ['mything', 'because of stuff']],
	['bind_invalid_parens', ['value']],
	['bind_invalid_target', ['value', '<input>, <select>, <textarea>']],
	['bind_invalid_expression', []],
	['bind_invalid_value', []],
	['bind_group_invalid_expression', []],
	['bind_group_invalid_snippet_parameter', []],
	['bindable_invalid_location', []]
];

const warning_cases = [
	// We pick warnings that are pure-function (no extra dependencies in state).
];

const results = [];

for (const [name, args] of error_cases) {
	try {
		errors[name](null, ...args);
		results.push({ kind: 'error', name, args, error: 'did not throw' });
	} catch (err) {
		results.push({
			kind: 'error',
			name,
			args,
			code: err.code,
			message: err.message,
			position: err.position ?? null
		});
	}
}

for (const [name, args] of warning_cases) {
	warning_state.length = 0;
	warnings[name](null, ...args);
	const w = warning_state[warning_state.length - 1];
	results.push({
		kind: 'warning',
		name,
		args,
		code: w.code,
		message: w.message,
		position: w.position ?? null
	});
}

process.stdout.write(JSON.stringify(results, null, '\t') + '\n');
