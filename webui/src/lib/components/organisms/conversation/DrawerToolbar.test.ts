import { afterEach, describe, expect, it, vi } from 'vitest';
import { mount, unmount } from 'svelte';
import DrawerToolbar from './DrawerToolbar.svelte';
import toolbarSource from './DrawerToolbar.svelte?raw';
import lineSource from './ConversationLine.svelte?raw';
import { allFilter } from './filters';
import type { ViewOpts } from './types';
import type { MessagePin } from '@bindings/MessagePin';

const flush = () => new Promise((r) => setTimeout(r, 0));

let comp: ReturnType<typeof mount> | null = null;
afterEach(() => {
	if (comp) unmount(comp);
	comp = null;
	document.body.innerHTML = '';
});

const view = (): ViewOpts =>
	({
		msgFilter: allFilter(true),
		prettyJson: true,
		prettyDiff: true,
		prettyTables: true,
		paneWidth: null
	}) as ViewOpts;

const pin = (seq: number): MessagePin => ({
	session_id: 's1',
	seq,
	message_id: null,
	note: null,
	created_at: '2026-01-01T00:00:00Z'
});

async function render(extra: Record<string, unknown> = {}) {
	comp = mount(DrawerToolbar, {
		target: document.body,
		props: {
			view: view(),
			autoApprove: false,
			ontoggleAuto: vi.fn(),
			onjumpseq: vi.fn(),
			onunpin: vi.fn(),
			...extra
		}
	});
	await flush();
	return document.querySelector('.toolbar') as HTMLElement;
}

describe('wrap-up bookmark shortcut', () => {
	it('is gone from the toolbar', () => {
		expect(toolbarSource).not.toContain('onbookmarkwrapup');
		expect(toolbarSource).not.toContain('bookmarks_toolbar');
	});
});

describe('popover triggers match their sibling toggles', () => {
	it('renders both triggers bare, with a local chip span carrying the chrome', async () => {
		const bar = await render();
		const triggers = bar.querySelectorAll('.pop-trigger.toolbar-chip');
		expect(triggers.length).toBe(2);
		for (const t of triggers) expect(t.classList.contains('bare')).toBe(true);
		const chips = [...triggers].map((t) => t.querySelector('.chip'));
		expect(chips.every(Boolean)).toBe(true);
		// Filters rides with the pill quick chips; Pins with the square toggles.
		expect(chips[0]!.classList.contains('pill')).toBe(true);
		expect(chips[1]!.classList.contains('pill')).toBe(false);
	});

	it('styles the chip with scoped CSS, never :global', () => {
		expect(toolbarSource).not.toContain(':global(');
	});

	it('drops the ad-hoc override string and the local label span', () => {
		expect(toolbarSource).not.toContain('chipTrigger');
		expect(toolbarSource).not.toContain('chip-label');
		expect(toolbarSource).not.toContain('--pop-trigger-');
		expect(toolbarSource).not.toContain('--pop-box');
	});

	it('restates every chrome declaration a Toggle sets', () => {
		const chrome = toolbarSource.slice(
			toolbarSource.indexOf('\t.chip {'),
			toolbarSource.indexOf('\t.chip.pill {')
		);
		for (const decl of [
			'padding: 0 var(--sp-2)',
			'border: 1px solid var(--border)',
			'border-radius: var(--r-sm)',
			'background: var(--bg-elevated-2)',
			'color: var(--text-muted)',
			'font-size: var(--fs-xs)',
			'font-weight: var(--fw-medium)',
			'line-height: 1'
		]) {
			expect(chrome).toContain(decl);
		}
	});
});

describe('pin glyph', () => {
	it('uses the kit pin icon, not a star, in the toolbar', async () => {
		const bar = await render();
		const chips = [...bar.querySelectorAll('.pop-trigger.toolbar-chip')];
		const pinChip = chips[chips.length - 1];
		expect(pinChip.textContent).not.toContain('★');
		expect(pinChip.querySelector('svg[data-tsu="Icon"]')).not.toBeNull();
	});

	it('renders outline when there are no pins and filled once there are', async () => {
		let bar = await render();
		let icon = [...bar.querySelectorAll('.pop-trigger.toolbar-chip svg')].pop() as SVGElement;
		expect(icon.getAttribute('fill')).toBe('none');

		if (comp) unmount(comp);
		document.body.innerHTML = '';
		bar = await render({ pins: [pin(3)] });
		icon = [...bar.querySelectorAll('.pop-trigger.toolbar-chip svg')].pop() as SVGElement;
		expect(icon.getAttribute('fill')).toBe('currentColor');
	});

	it('uses the same icon for the per-message action', () => {
		expect(lineSource).not.toContain("pinned ? '★' : '☆'");
		expect(lineSource).toContain('<Icon name="pin" size={16} filled={pinned} />');
	});
});

describe('terminal toggle', () => {
	it('is not in the toolbar', async () => {
		const bar = await render({});
		expect(bar.textContent).not.toMatch(/terminal/i);
		expect(toolbarSource).not.toContain('onterminal');
	});
});

describe('single-row toolbar', () => {
	it('has no format toggles, no diagnose and no mobile tabs', async () => {
		const bar = await render({});
		expect(bar.textContent).not.toMatch(/JSON|Diff|Tables|Diagnose/);
		expect(bar.querySelector('[data-journey="mobile-panel"]')).toBeNull();
		expect(toolbarSource).not.toContain('mobilePanel');
	});

	it('marks auto-approve with a zap and keeps its accessible name', async () => {
		const bar = await render({});
		const auto = bar.querySelector('.behbar button') as HTMLElement;
		expect(auto.textContent).toContain('⚡');
		expect(auto.getAttribute('aria-label')).toBeTruthy();
	});

	it('shows how many categories are hidden next to the filter icon', async () => {
		const bar = await render({
			view: { ...view(), msgFilter: { ...allFilter(true), thinking: false, marker: false } }
		});
		const trigger = bar.querySelector('[data-journey="filter-menu"]') as HTMLElement;
		expect(trigger.querySelector('svg')).toBeTruthy();
		expect(trigger.querySelector('.narrow')?.textContent).toBe('2');
	});
});

describe('drawer toolbar sizing', () => {
	const css = toolbarSource.slice(toolbarSource.indexOf('<style>'));

	it('switches form on the bar width, not the viewport', () => {
		expect(css).toContain('container: drawer-toolbar / inline-size');
		expect(css).toContain('@container drawer-toolbar (max-width: 1000px)');
		expect(css).not.toContain('@media');
	});

	it('drops the labels and floats the behaviour group in the compact form', () => {
		const q = css.slice(css.indexOf('@container drawer-toolbar'));
		const body = q.slice(0, q.indexOf('\n\t}'));
		expect(body).toContain('.wide {');
		expect(body).toContain('.narrow {');
		expect(body).toContain('margin-left: auto');
	});

	it('never lets auto-approve or pins be the group that clips', () => {
		expect(css).toMatch(/\.behbar,\n\t\.hitbar \{\n\t\tflex: none;/);
		const tag = css.slice(css.indexOf('.tagbar {'));
		const body = tag.slice(0, tag.indexOf('}'));
		expect(body).toContain('min-width: 0');
		expect(body).toContain('overflow: hidden');
	});

	it('pins every control in the bar to one height', () => {
		expect(css).toContain('--bar-ctl-h: 24px');
		expect(toolbarSource).toContain(
			"const CTL = 'height:var(--bar-ctl-h);box-sizing:border-box;padding-block:0;line-height:1'"
		);
		expect(toolbarSource).toContain("const TRIG = 'display:flex;align-items:center;height:var(--bar-ctl-h)'");
		const chip = toolbarSource.slice(toolbarSource.indexOf('\t.chip {'), toolbarSource.indexOf('\t.chip.pill {'));
		expect(chip).toContain('height: var(--bar-ctl-h)');
		expect(chip).toContain('box-sizing: border-box');
	});

	it('gives auto-approve and pins the same box construction', () => {
		const toggles = toolbarSource.match(/<Toggle\b[\s\S]*?>/g) ?? [];
		expect(toggles.length).toBeGreaterThanOrEqual(3);
		for (const t of toggles) expect(t, t.slice(0, 60)).toContain('style={');
		for (const t of toggles) expect(t, t.slice(0, 60)).toMatch(/\$\{CTL\}|style=\{CTL\}/);
		const triggers = toolbarSource.match(/<Popover[\s\S]*?>/g) ?? [];
		expect(triggers.length).toBe(2);
		for (const p of triggers) expect(p).toContain('style={TRIG}');
	});

	it('neutralises the emoji line box so it cannot set the height', () => {
		expect(toolbarSource).toContain('<span class="glyph" aria-hidden="true">⚡</span>');
		const glyph = toolbarSource.slice(toolbarSource.indexOf('\t.glyph {'));
		const body = glyph.slice(0, glyph.indexOf('}'));
		expect(body).toContain('line-height: 1');
		expect(body).toContain('min-width: 1em');
	});

	it('matches the two chip icons in size', () => {
		expect(toolbarSource).toContain('<Icon name="filter" size={12} />');
		expect(toolbarSource).toContain('<Icon name="pin" size={12}');
	});

	it('keeps the compact ⚡ and 📌 named and right-aligned', () => {
		expect(toolbarSource).toContain('aria-label={m.conversation_auto_approve_aria()}');
		expect(toolbarSource).toContain('title={m.conversation_auto_approve_title()}');
		expect(toolbarSource).toContain('label={m.conversation_pins_aria()}');
		const q = css.slice(css.indexOf('@container drawer-toolbar'));
		expect(q.slice(0, q.indexOf('\n\t}'))).toContain('margin-left: auto');
	});

	it('adds no :global override', () => {
		expect(toolbarSource).not.toContain(':global(');
	});
});
