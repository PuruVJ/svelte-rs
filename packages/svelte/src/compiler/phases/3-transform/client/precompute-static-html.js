/** @import { AST, ComponentAnalysis } from '#compiler' */
/** @import { ComponentClientTransformState } from './types' */
import { escape_html } from '../../../../escaping.js';
import { is_void } from '../../../../utils.js';
import { is_text_attribute } from '../../../utils/ast.js';
import { compute_is_static_element } from '../../2-analyze/mark-static-elements.js';
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
 */
function is_fully_static_subtree(node) {
	if (node.type !== 'RegularElement') return false;
	if (!compute_is_static_element(node)) return false;

	for (const child of node.fragment.nodes) {
		if (child.type === 'RegularElement' && !is_fully_static_subtree(child)) {
			return false;
		}
	}

	return true;
}

/**
 * @param {AST.RegularElement} node
 * @param {AST.SvelteNode[]} path
 * @param {ComponentAnalysis} analysis
 * @param {Pick<ComponentClientTransformState, 'analysis' | 'options' | 'preserve_whitespace'>} state
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
		/** @type {any} */ ({ analysis: state.analysis, options: state.options }),
		state.preserve_whitespace || node.name === 'pre' || node.name === 'textarea',
		state.options.preserveComments
	);

	let inner = '';
	for (const child of trimmed) {
		inner += serialize_cleaned_node(child, path, analysis, state, child_namespace);
	}

	const open = open_tag(node, is_html, css_hash);
	if (open.endsWith('/>')) return open;
	return open + inner + close_tag(node, is_html);
}

/**
 * @param {AST.SvelteNode} node
 * @param {AST.SvelteNode[]} path
 * @param {ComponentAnalysis} analysis
 * @param {Pick<ComponentClientTransformState, 'analysis' | 'options' | 'preserve_whitespace'>} state
 * @param {import('#compiler').Namespace} namespace
 */
function serialize_cleaned_node(node, path, analysis, state, namespace) {
	if (node.type === 'Text') {
		return node.raw ?? node.data;
	}

	if (node.type === 'Comment') {
		return node.data ? `<!--${node.data}-->` : '<!>';
	}

	if (node.type === 'RegularElement' && is_fully_static_subtree(node)) {
		return serialize_fully_static_element(node, [...path, node], analysis, state, namespace);
	}

	return '';
}

/**
 * Fill `metadata.cached_static_html` using the same whitespace rules as transform `clean_nodes`.
 * @param {ComponentAnalysis} analysis
 * @param {Pick<ComponentClientTransformState, 'analysis' | 'options' | 'preserve_whitespace'>} state
 */
export function precompute_static_html_cache(analysis, state) {
	const namespace = state.options.namespace ?? 'html';
	walk_nodes(analysis.template.ast.nodes, [analysis.template.ast], analysis, state, namespace);
}

/**
 * @param {AST.SvelteNode[]} nodes
 * @param {AST.SvelteNode[]} path
 * @param {ComponentAnalysis} analysis
 * @param {Pick<ComponentClientTransformState, 'analysis' | 'options' | 'preserve_whitespace'>} state
 * @param {import('#compiler').Namespace} namespace
 */
function walk_nodes(nodes, path, analysis, state, namespace) {
	for (const node of nodes) {
		walk_node(node, path, analysis, state, namespace);
	}
}

/**
 * @param {AST.SvelteNode} node
 * @param {AST.SvelteNode[]} path
 * @param {ComponentAnalysis} analysis
 * @param {Pick<ComponentClientTransformState, 'analysis' | 'options' | 'preserve_whitespace'>} state
 * @param {import('#compiler').Namespace} namespace
 */
function walk_node(node, path, analysis, state, namespace) {
	switch (node.type) {
		case 'RegularElement': {
			if (is_fully_static_subtree(node)) {
				node.metadata.cached_static_html = serialize_fully_static_element(
					node,
					path,
					analysis,
					state,
					namespace
				);
			}

			const child_namespace =
				node.name === 'foreignObject' || (namespace === 'html' && node.name === 'svg')
					? 'html'
					: node.name === 'svg' || namespace === 'svg'
						? 'svg'
						: namespace;
			walk_nodes(node.fragment.nodes, [...path, node], analysis, state, child_namespace);
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
			walk_nodes(node.fragment.nodes, [...path, node], analysis, state, namespace);
			break;
		case 'IfBlock':
			walk_nodes(node.consequent.nodes, [...path, node], analysis, state, namespace);
			if (node.alternate) walk_nodes(node.alternate.nodes, [...path, node], analysis, state, namespace);
			break;
		case 'EachBlock':
			walk_nodes(node.body.nodes, [...path, node], analysis, state, namespace);
			if (node.fallback) walk_nodes(node.fallback.nodes, [...path, node], analysis, state, namespace);
			break;
		case 'KeyBlock':
			walk_nodes(node.fragment.nodes, [...path, node], analysis, state, namespace);
			break;
		case 'AwaitBlock':
			if (node.pending) walk_nodes(node.pending.nodes, [...path, node], analysis, state, namespace);
			if (node.then) walk_nodes(node.then.nodes, [...path, node], analysis, state, namespace);
			if (node.catch) walk_nodes(node.catch.nodes, [...path, node], analysis, state, namespace);
			break;
		case 'SnippetBlock':
			walk_nodes(node.body.nodes, [...path, node], analysis, state, namespace);
			break;
		default:
			break;
	}
}
