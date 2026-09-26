import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import type { TokenUsage } from '@bindings/TokenUsage';
import { bustReasonKey, tokenUsageLayout, tokenUsageTitle } from './TokenUsage.logic';

const usage = (over: Partial<TokenUsage> = {}): TokenUsage =>
	({
		tokens_in: 1000,
		tokens_out: 200,
		cache_read_tokens: 5000,
		cache_creation_tokens: 300,
		cost_usd: 1.25,
		...over
	}) as unknown as TokenUsage;

const busted = (reason = 'gateway_rewrote_body'): TokenUsage =>
	usage({ cache_bust: { lost_tokens: 172_523, lost_usd: 2.98, reason } });

describe('tokenUsageLayout', () => {
	it('sums in + out + cache for Σ and shows every segment by default', () => {
		const l = tokenUsageLayout(usage());
		expect(l.total).toBe(6500);
		expect(l.cacheTotal).toBe(5300);
		expect(l.cost).toBe(1.25);
		expect(l.sumMode).toBe('always');
		expect(l.showCache).toBe(true);
		expect(l.showCost).toBe(true);
		expect(l.showCold).toBe(false);
		expect(l.showBust).toBe(false);
	});

	it('flags a bust only when the server sent one', () => {
		expect(tokenUsageLayout(busted()).showBust).toBe(true);
		expect(tokenUsageLayout(usage()).showBust).toBe(false);
	});

	it('lets `sum` override Σ without touching the breakdown', () => {
		const l = tokenUsageLayout(usage(), { sum: 99_000 });
		expect(l.total).toBe(99_000);
		expect(l.cacheTotal).toBe(5300);
	});

	it('keeps Σ for the compact form when showSum is false', () => {
		expect(tokenUsageLayout(usage(), { showSum: false }).sumMode).toBe('compact-only');
	});

	it('never renders Σ without a total, whatever showSum says', () => {
		const empty = usage({ tokens_in: 0, tokens_out: 0, cache_read_tokens: 0, cache_creation_tokens: 0 });
		expect(tokenUsageLayout(empty).sumMode).toBe('never');
		expect(tokenUsageLayout(empty, { showSum: false }).sumMode).toBe('never');
		expect(tokenUsageLayout(usage(), { sum: 0 }).sumMode).toBe('never');
	});

	it('hides cache, cost and cold when they carry nothing', () => {
		const l = tokenUsageLayout(usage({ cache_read_tokens: 0, cache_creation_tokens: 0, cost_usd: 0 }));
		expect(l.showCache).toBe(false);
		expect(l.showCost).toBe(false);
		expect(l.total).toBe(1200);
		expect(tokenUsageLayout(usage({ cost_usd: null as unknown as number })).cost).toBe(0);
		expect(tokenUsageLayout(usage(), { cold: true }).showCold).toBe(true);
	});
});

describe('tokenUsageTitle', () => {
	const fmt = { num: (n: number) => String(n), usd: (n: number) => `$${n}` };

	it('spells the whole readout out, Σ first, cold last', () => {
		const u = usage();
		expect(tokenUsageTitle(u, tokenUsageLayout(u, { cold: true }), fmt)).toBe(
			'Σ6500 ↑1000 ↓200 ⚡5300 $1.25 ❄️'
		);
	});

	it('omits the segments the layout hides', () => {
		const u = usage({ cache_read_tokens: 0, cache_creation_tokens: 0, cost_usd: 0 });
		expect(tokenUsageTitle(u, tokenUsageLayout(u), fmt)).toBe('Σ1200 ↑1000 ↓200');
		const empty = usage({ tokens_in: 0, tokens_out: 0, cache_read_tokens: 0, cache_creation_tokens: 0 });
		expect(tokenUsageTitle(empty, tokenUsageLayout(empty), fmt)).toBe('↑0 ↓0 $1.25');
	});

	it('appends 💥 after the cold marker on a bust turn', () => {
		const b = busted();
		expect(tokenUsageTitle(b, tokenUsageLayout(b, { cold: true }), fmt)).toBe(
			'Σ6500 ↑1000 ↓200 ⚡5300 $1.25 ❄️ 💥'
		);
		expect(tokenUsageTitle(usage(), tokenUsageLayout(usage()), fmt)).not.toContain('💥');
	});
});

describe('bustReasonKey', () => {
	it('passes the known reasons through and folds anything else to unknown', () => {
		expect(bustReasonKey('ttl_expired')).toBe('ttl_expired');
		expect(bustReasonKey('gateway_rewrote_body')).toBe('gateway_rewrote_body');
		expect(bustReasonKey('unknown')).toBe('unknown');
		expect(bustReasonKey('something_new')).toBe('unknown');
		expect(bustReasonKey('')).toBe('unknown');
	});
});

// The degradation itself is CSS (a container query cannot be evaluated in a
// layout-less DOM), so what a unit test CAN guard is the wiring that made
// CCT-846 regress: the molecule querying host containers that nobody declared.
const src = (rel: string) => readFileSync(new URL(rel, import.meta.url), 'utf8');
const containerBlock = (css: string, name: string) =>
	css.match(new RegExp(`@container ${name} \\(max-width:[^)]+\\) \\{[\\s\\S]*?\\n\\t\\}`))?.[0] ?? '';

describe('cramped-container degradation', () => {
	const svelte = src('./TokenUsage.svelte');

	it.each(['sess-card', 'drawer-head'])('drops the ↑↓⚡ detail and keeps Σ inside a cramped %s', (name) => {
		const block = containerBlock(svelte, name);
		expect(block).toMatch(/\.detail \{\s*display: none;/);
		expect(block).toMatch(/\.sum-compact-only \{\s*display: contents;/);
	});

	it('keeps only Σ inside a cramped drawer-head', () => {
		expect(containerBlock(svelte, 'drawer-head')).toMatch(/\.cost \{\s*display: none;/);
	});

	it('drops the $ cost as the last step, keeping only Σ', () => {
		const block = svelte.match(/@container sess-card \(max-width: 16rem\) \{[\s\S]*?\n\t\}/)?.[0] ?? '';
		expect(block).toMatch(/\.cost \{\s*display: none;/);
		expect(block).not.toContain('.sum-compact-only');
	});

	it('the Σ tooltip carries the full readout', () => {
		expect(svelte).toContain('tokenUsageTitle(usage, layout, { num: compactNum, usd })');
	});

	it('has the hosts that declare those containers', () => {
		expect(src('../organisms/SessionCard.svelte')).toContain('container: sess-card / inline-size;');
		expect(src('../organisms/conversation/DrawerHeader.svelte')).toContain('container: drawer-head / inline-size;');
	});

	it('renders Σ in the degraded form even when the mount opted out of it', () => {
		expect(tokenUsageLayout(usage(), { showSum: false }).sumMode).toBe('compact-only');
		expect(svelte).toContain('class:sum-compact-only={layout.sumMode === \'compact-only\'}');
	});
});
