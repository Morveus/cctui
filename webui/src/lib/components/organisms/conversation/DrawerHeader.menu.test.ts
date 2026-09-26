import { describe, expect, it } from 'vitest';
import header from './DrawerHeader.svelte?raw';
import toolbar from './DrawerToolbar.svelte?raw';
import drawer from '../ConversationDrawer.svelte?raw';

const items = () => {
	const start = header.indexOf('const overflowItems');
	expect(start).toBeGreaterThan(-1);
	return header.slice(start, header.indexOf(']);', start));
};

const markup = () => header.slice(header.indexOf('</script>'));

describe('drawer header ⋯ menu', () => {
	it('renders its own always-present menu, not the collapse-only kit overflow', () => {
		expect(markup()).toContain('<Menu label={m.drawer_more_actions()} items={overflowItems}');
		expect(markup()).not.toMatch(/<Toolbar[^>]*\bitems=/);
		const menuAt = markup().indexOf('<Menu');
		const lastIf = markup().lastIndexOf('{#if', menuAt);
		const lastEnd = markup().lastIndexOf('{/if}', menuAt);
		expect(lastEnd).toBeGreaterThan(lastIf);
	});

	it('holds copy link, fork and the terminal permanently', () => {
		const list = items();
		expect(list).toContain('m.drawer_copy_link_label()');
		expect(list).toContain('m.drawer_fork_label()');
		expect(list).toContain('pressed: onforkselect ? forkSelectActive : undefined');
		expect(list).toContain('m.drawer_terminal_label()');
		expect(list).toContain('pressed: terminalOpen');
		expect(list).toContain("'data-journey': 'terminal'");
	});

	it('only stands in for rename while the bar is collapsed', () => {
		expect(items()).toMatch(/\.\.\.\(collapsed\s*\?\s*\[\s*renaming/);
		expect(header).toContain('collapseBelow="{COLLAPSE_BELOW}px"');
	});

	it('no longer renders the moved actions inline', () => {
		for (const icon of ['link', 'markdown', 'download', 'fork']) {
			expect(markup(), icon).not.toMatch(new RegExp(`<IconButton[^>]*icon="${icon}"`));
		}
	});

	it('gives every entry an icon, checkable ones included', () => {
		const entries = items()
			.split(/\blabel:/)
			.slice(1);
		expect(entries.length).toBeGreaterThanOrEqual(7);
		for (const e of entries) expect(e, e.slice(0, 40)).toMatch(/\bicon:/);
		expect(items()).toContain("icon: 'recycle' as const");
		expect(header).toMatch(/followupItem = \$derived<MenuItem \| null>\([\s\S]*?icon: 'arrow-right'/);
	});

	it('anchors the tour on the menu trigger', () => {
		expect(markup()).toContain('data-journey="actions"');
		expect(markup()).not.toContain('data-journey="fork"');
	});

	it('wires the terminal to the header, not the toolbar', () => {
		const head = drawer.slice(drawer.indexOf('<DrawerHeader'), drawer.indexOf('/>', drawer.indexOf('<DrawerHeader')));
		const bar = drawer.slice(drawer.indexOf('<DrawerToolbar'), drawer.indexOf('/>', drawer.indexOf('<DrawerToolbar')));
		expect(head).toContain('onterminal=');
		expect(head).toContain('{terminalOpen}');
		expect(bar).not.toContain('terminal');
		expect(toolbar).not.toContain('terminal');
	});

	it('adds no :global override', () => {
		expect(header).not.toContain(':global(');
	});
});
