/** @import { AST } from '#compiler' */
import { cannot_be_set_statically } from '../../../utils.js';
import { is_event_attribute, is_text_attribute } from '../../utils/ast.js';
import { is_custom_element_node } from '../nodes.js';

/**
 * Whether a regular element can use static-subtree skipping at transform time.
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
 * Set static-subtree metadata after children have been analyzed.
 * @param {AST.RegularElement} node
 */
export function mark_element_static_metadata(node) {
	node.metadata.is_static_element = compute_is_static_element(node);
	node.metadata.is_fully_static_subtree =
		node.metadata.is_static_element &&
		node.fragment.nodes.every(
			(child) => child.type !== 'RegularElement' || child.metadata.is_fully_static_subtree === true
		);
}
