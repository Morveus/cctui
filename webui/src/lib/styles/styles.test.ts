import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';
import { THEMES } from '$lib/theme.svelte';

const KIT = 'node_modules/@dorsk/tsumikit/dist/styles';
const read = (p: string) => readFileSync(p, 'utf8');
const variables = read('src/lib/styles/variables.css');
const app = read('src/lib/styles/app.css');
const tokens = read(`${KIT}/tokens.css`);
const themes = read(`${KIT}/themes.css`);

type Decls = Map<string, string>;
function block(css: string, selector: string): Decls {
	const start = css.indexOf(`${selector} {`);
	expect(start, `${selector} block`).toBeGreaterThanOrEqual(0);
	const body = css.slice(start, css.indexOf('\n}', start));
	const out: Decls = new Map();
	for (const m of body.matchAll(/(--[\w-]+):\s*([^;]+);/g)) out.set(m[1], m[2].trim());
	return out;
}
function resolve(name: string, scopes: Decls[], depth = 0): string {
	expect(depth, `cycle resolving ${name}`).toBeLessThan(10);
	const raw = scopes.map((s) => s.get(name)).find((v) => v !== undefined);
	if (raw === undefined) throw new Error(`${name} is not declared`);
	return raw.replace(/var\((--[\w-]+)\)/g, (_, ref) => resolve(ref, scopes, depth + 1));
}

const PALETTE = [
	'bg', 'bg-elev', 'bg-elev-2', 'surface', 'border', 'border-strong', 'text', 'text-muted',
	'text-faint', 'accent', 'accent-ink', 'accent-dim', 'blue', 'amber', 'red', 'green', 'violet',
	'gold', 'teal'
].map((t) => `--c-${t}`);

describe('variables.css is a kit import, not a fork', () => {
	it('imports the kit tokens and themes and declares no palette of its own', () => {
		expect(variables).toContain('@import "@dorsk/tsumikit/styles/tokens.css";');
		expect(variables).toContain('@import "@dorsk/tsumikit/styles/themes.css";');
		expect(variables).not.toMatch(/\[data-theme=/);
		for (const t of PALETTE) expect(variables).not.toContain(`${t}:`);
		expect(variables).not.toContain('--box-');
		expect(variables).not.toContain('--control-height');
	});

	it('keeps only the app-specific tokens', () => {
		const root = block(variables, ':root');
		expect([...root.keys()].sort()).toEqual(
			[
				'--attention-bg-solid',
				'--c-brown',
				'--content-wide',
				'--role-peer',
				'--role-poll',
				'--role-queued',
				'--role-summary',
				'--role-thinking',
				'--term-bg'
			]
		);
	});

	it('gets the box scale and touch target from the kit', () => {
		const root = block(tokens, ':root');
		for (const t of ['--box-xs', '--box-sm', '--box-md', '--box-lg', '--touch-target', '--control-height-compact', '--control-height-default', '--control-height-large'])
			expect(root.has(t), t).toBe(true);
	});
});

describe('every app theme resolves against the kit stylesheet', () => {
	const root = block(tokens, ':root');
	const ids = THEMES.map((t) => t.id).filter((id) => id !== 'dark');

	it.each(ids)('%s has a kit block with the full palette contract', (id) => {
		const theme = block(themes, `[data-theme="${id}"]`);
		for (const t of PALETTE) expect(theme.has(t), `${id} ${t}`).toBe(true);
		for (const t of ['--shadow-sm', '--shadow-md', '--shadow-lg', '--mach-bg-sl', '--mach-fg-sl', '--mach-border-sl'])
			expect(theme.has(t), `${id} ${t}`).toBe(true);
	});

	it.each(['dark', ...ids])('%s resolves Button tone=success ink and the app tokens to literals', (id) => {
		const scopes = id === 'dark' ? [root] : [block(themes, `[data-theme="${id}"]`), root];
		for (const t of ['--text-on-success', '--text-on-accent', '--accent', '--ok'])
			expect(resolve(t, scopes)).toMatch(/^(#[0-9a-f]{3,8}|rgba?\(|hsla?\()/i);
		const appScopes = [block(variables, ':root'), ...scopes];
		expect(resolve('--role-thinking', appScopes)).not.toMatch(/var\(/);
		expect(resolve('--attention-bg-solid', appScopes)).not.toMatch(/var\(/);
	});
});

describe('app.css no longer re-declares kit classes', () => {
	it('imports the kit reset and utilities', () => {
		expect(app).toContain('@import "@dorsk/tsumikit/styles/reset.css";');
		expect(app).toContain('@import "@dorsk/tsumikit/styles/utilities.css";');
	});
	it.each(['.btn-icon', '.btn-icon-inline', '.btn-control', '.btn-control-square', '.card', '.card-tap', '.container', '.stack', '.row', '.sr-only', '.divider', '.empty'])(
		'%s is not declared',
		(cls) => {
			expect(app).not.toMatch(new RegExp(`^${cls.replace('.', '\\.')}\\s*[{,]`, 'm'));
		}
	);
	it('renamed the empty placeholder away from the kit EmptyState class', () => {
		expect(app).toMatch(/^\.placeholder \{/m);
	});
});

function walk(dir: string, out: string[] = []): string[] {
	for (const name of readdirSync(dir)) {
		const p = join(dir, name);
		if (statSync(p).isDirectory()) walk(p, out);
		else if (/\.(svelte|css|js)$/.test(name)) out.push(p);
	}
	return out;
}

describe('every token the kit reads resolves from the imported stylesheets', () => {
	const declared = new Set<string>();
	for (const css of [tokens, themes, variables])
		for (const m of css.matchAll(/(--[\w-]+):/g)) declared.add(m[1]);
	const kitFiles = walk(`node_modules/@dorsk/tsumikit/dist/components`);
	// Set on the element by the component itself (style:--x / inline style).
	const local = new Set<string>();
	for (const f of kitFiles)
		for (const m of readFileSync(f, 'utf8').matchAll(/(?:style:|style="|`|;\s*|^\s*)(--[\w-]+)\s*[:=]/gm)) local.add(m[1]);
	// Consumer-provided geometry; the kit reads them without a fallback on
	// purpose so the shell owner can size the docks.
	const consumer = new Set(['--dock-left-w', '--dock-right-w']);

	it('leaves no fallback-less var() unresolved', () => {
		const missing = new Set<string>();
		for (const f of kitFiles)
			for (const m of readFileSync(f, 'utf8').matchAll(/var\((--[\w-]+)\)/g)) {
				const name = m[1];
				if (name.endsWith('-') || declared.has(name) || local.has(name) || consumer.has(name)) continue;
				missing.add(name);
			}
		expect([...missing]).toEqual([]);
	});

	it.each(['--control-height', '--control-height-compact', '--control-height-default', '--control-height-large', '--box-xs', '--box-sm', '--box-md', '--box-lg', '--touch-target', '--text-on-success'])(
		'%s resolves to a literal length or colour',
		(name) => {
			expect(resolve(name, [block(tokens, ':root')])).toMatch(/^(\d|#|rgb|hsl)/);
		}
	);
});

describe('the journey overlay is themed from the palette, not the library fallbacks', () => {
	const overlay = read('node_modules/@dorsk/journey/dist/runtime/overlay.js');
	const fallbacks = new Map<string, string>();
	for (const m of overlay.matchAll(/var\((--journey-[\w-]+),\s*([^()]*(?:\([^()]*\)[^()]*)*)\)/g))
		fallbacks.set(m[1], m[2].trim());
	const host = block(app, 'journey-overlay');
	const root = block(tokens, ':root');

	it('reads at least the accent, surface and text tokens', () => {
		for (const t of ['--journey-accent', '--journey-surface', '--journey-text'])
			expect(fallbacks.has(t), t).toBe(true);
	});

	it('maps every token the package reads, and nothing it does not', () => {
		expect([...host.keys()].sort()).toEqual([...fallbacks.keys()].sort());
	});

	// Resolved values cannot be compared to the fallbacks: --r-sm is 6px and so
	// is the library's. Sourcing from a palette var() is the check that holds.
	// A keyword, not a colour or a measure: the palette has no analogue to source it from.
	const KEYWORD_TOKENS = new Set(['--journey-banner-align']);

	it('sources every token from the palette rather than a literal of its own', () => {
		for (const [name, value] of host)
			if (!KEYWORD_TOKENS.has(name)) expect(value, name).toMatch(/var\(--/);
	});

	it.each(['dark', ...THEMES.map((t) => t.id).filter((id) => id !== 'dark')])(
		'%s resolves every token to a literal',
		(id) => {
			const scopes = [host, ...(id === 'dark' ? [] : [block(themes, `[data-theme="${id}"]`)]), root];
			for (const name of fallbacks.keys())
				expect(resolve(name, scopes), `${id} ${name}`).not.toMatch(/var\(/);
		}
	);
});
