/** @import { AST, ComponentAnalysis } from '#compiler' */
/** @import { ComponentClientTransformState, ComponentContext } from './types' */
import { escape_html } from '../../../../escaping.js';
import { is_void } from '../../../../utils.js';
import { is_text_attribute } from '../../../utils/ast.js';
import { clean_nodes } from '../utils.js';

/**
 * @param {AST.Attribute['value']} value
 */
function attribute_value_string(value) {
	if (value === true) return '';
	if (typeof value === 'string') return value;
	if (Array.isArray(value)) {
		return value
			.map((part) => {
				if (part.type === 'Text') return part.data;
				return '';
			})
			.join('');
	}
	return '';
}

/**
 * @param {AST.RegularElement} node
 * @param {boolean} is_html
 * @param {string | null | undefined} css_hash
 */
function open_tag(node, is_html, css_hash) {
	const name = is_html ? node.name.toLowerCase() : node.name;
	let str = `<${name}`;

	for (const attribute of node.attributes) {
		if (attribute.type !== 'Attribute') continue;
		const key = is_html ? attribute.name.toLowerCase() : attribute.name;
		if (attribute.value === true) {
			if (key === 'class' && node.metadata.scoped && css_hash) {
				str += ` ${key}="${escape_html(css_hash, true)}"`;
			} else {
				str += ` ${key}`;
			}
		} else if (is_text_attribute(attribute)) {
			let val = attribute_value_string(attribute.value);
			if (key === 'class' && node.metadata.scoped && css_hash) {
				val = val ? `${val} ${css_hash}` : css_hash;
			}
			str += ` ${key}="${escape_html(val, true)}"`;
		}
	}

	if (is_void(name)) {
		return str + '/>';
	}

	return str + '>';
}

/**
 * @param {AST.RegularElement} node
 * @param {boolean} is_html
 */
function close_tag(node, is_html) {
	const name = is_html ? node.name.toLowerCase() : node.name;
	return `</${name}>`;
}

/**
 * @param {AST.SvelteNode} node
 * @param {AST.SvelteNode[]} path
 * @param {ComponentAnalysis} analysis
 * @param {ComponentClientTransformState} state
 * @param {import('#compiler').Namespace} namespace
 */
function serialize_cleaned_node(node, path, analysis, state, namespace) {
	if (node.type === 'Text') {
		return node.raw ?? node.data;
	}

	if (node.type === 'Comment') {
		return node.data ? `<!--${node.data}-->` : '<!>';
	}

	if (node.type === 'RegularElement' && node.metadata.is_fully_static_subtree) {
		const cached = node.metadata.cached_static_html;
		if (cached) return cached;
		return serialize_fully_static_element(node, path, analysis, state, namespace);
	}

	return '';
}

/**
 * @param {AST.RegularElement} node
 * @param {AST.SvelteNode[]} path
 * @param {ComponentAnalysis} analysis
 * @param {ComponentClientTransformState} state
 * @param {import('#compiler').Namespace} namespace
 */
function serialize_fully_static_element(node, path, analysis, state, namespace) {
	const is_html = namespace === 'html' && node.name !== 'svg';
	const css_hash = analysis.css.hash || null;
	const child_namespace =
		node.name === 'foreignObject' || (namespace === 'html' && node.name === 'svg')
			? 'html'
			: node.name === 'svg' || namespace === 'svg'
				? 'svg'
				: namespace;

	const { trimmed } = clean_nodes(
		node,
		node.fragment.nodes,
		path,
		child_namespace,
		state,
		state.preserve_whitespace || node.name === 'pre' || node.name === 'textarea',
		state.options.preserveComments
	);

	let inner = '';
	for (const child of trimmed) {
		inner += serialize_cleaned_node(child, [...path, node], analysis, state, child_namespace);
	}

	const open = open_tag(node, is_html, css_hash);
	if (open.endsWith('/>')) return open;
	return open + inner + close_tag(node, is_html);
}

/**
 * Lazily serialize a fully-static element subtree for template HTML injection.
 * @param {AST.RegularElement} node
 * @param {ComponentContext} context
 */
export function serialize_fully_static_element_for_template(node, context) {
	const html = serialize_fully_static_element(
		node,
		context.path,
		context.state.analysis,
		context.state,
		context.state.metadata.namespace
	);
	node.metadata.cached_static_html = html;
	return html;
}
