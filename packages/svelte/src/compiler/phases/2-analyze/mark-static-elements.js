/** @import { AST } from '#compiler' */
import { cannot_be_set_statically } from '../../../utils.js';
import { is_event_attribute, is_text_attribute } from '../../utils/ast.js';
import { is_custom_element_node } from '../nodes.js';

/**
 * Whether a regular element can use static-subtree skipping at transform time.
 * Mirrors `phases/3-transform/client/visitors/shared/fragment.js` (pre-Rust port).
 * @param {AST.RegularElement} node
 */
export function compute_is_static_element(node) {
	if (node.fragment.metadata.dynamic) return false;
	if (is_custom_element_node(node)) return false;

	for (const attribute of node.attributes) {
		if (attribute.type !== 'Attribute') {
			return false;
		}

		if (is_event_attribute(attribute)) {
			return false;
		}

		if (cannot_be_set_statically(attribute.name)) {
			return false;
		}

		if (attribute.name === 'dir') {
			return false;
		}

		if (
			['input', 'textarea'].includes(node.name) &&
			['value', 'checked'].includes(attribute.name)
		) {
			return false;
		}

		if (node.name === 'option' && attribute.name === 'value') {
			return false;
		}

		if (node.name === 'img' && attribute.name === 'loading') {
			return false;
		}

		if (attribute.value !== true && !is_text_attribute(attribute)) {
			return false;
		}
	}

	return true;
}

/**
 * @param {AST.SvelteNode[]} nodes
 */
function walk_nodes(nodes) {
	for (const node of nodes) {
		mark_node(node);
	}
}

/**
 * @param {AST.SvelteNode} node
 */
function mark_node(node) {
	switch (node.type) {
		case 'RegularElement': {
			node.metadata.is_static_element = compute_is_static_element(node);
			walk_nodes(node.fragment.nodes);
			break;
		}
		case 'Component':
		case 'SlotElement':
		case 'TitleElement':
		case 'SvelteElement':
		case 'SvelteComponent':
		case 'SvelteBody':
		case 'SvelteBoundary':
		case 'SvelteDocument':
		case 'SvelteFragment':
		case 'SvelteHead':
		case 'SvelteWindow':
		case 'SvelteSelf':
		case 'SvelteOptions':
			walk_nodes(node.fragment.nodes);
			break;
		case 'IfBlock':
			walk_nodes(node.consequent.nodes);
			if (node.alternate) walk_nodes(node.alternate.nodes);
			break;
		case 'EachBlock':
			walk_nodes(node.body.nodes);
			if (node.fallback) walk_nodes(node.fallback.nodes);
			break;
		case 'KeyBlock':
			walk_nodes(node.fragment.nodes);
			break;
		case 'AwaitBlock':
			if (node.pending) walk_nodes(node.pending.nodes);
			if (node.then) walk_nodes(node.then.nodes);
			if (node.catch) walk_nodes(node.catch.nodes);
			break;
		case 'SnippetBlock':
			walk_nodes(node.body.nodes);
			break;
		default:
			break;
	}
}

/** @param {AST.Fragment} fragment */
export function mark_static_elements(fragment) {
	walk_nodes(fragment.nodes);
}
