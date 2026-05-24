/** @import { AST } from '#compiler' */
import {
	regex_ends_with_whitespaces,
	regex_starts_with_whitespaces
} from '../patterns.js';

/**
 * Mutate leading/trailing whitespace on boundary text nodes (analyze-time).
 * Gated by `experimental.compiler.trimBoundaryText` — experimental until
 * snapshot-validated for all fixtures.
 * @param {AST.Fragment} fragment
 */
export function trim_fragment_boundary_text(fragment) {
	const nodes = fragment.nodes;
	if (nodes.length === 0) return;

	const first = nodes[0];
	if (first?.type === 'Text') {
		first.data = first.data.replace(regex_starts_with_whitespaces, '');
		first.raw = first.raw.replace(regex_starts_with_whitespaces, '');
	}

	const last = nodes[nodes.length - 1];
	if (last?.type === 'Text' && last !== first) {
		last.data = last.data.replace(regex_ends_with_whitespaces, '');
		last.raw = last.raw.replace(regex_ends_with_whitespaces, '');
	}

	fragment.metadata.boundary_trimmed = true;
}
