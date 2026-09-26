import { flushSync, mount, tick, unmount } from 'svelte';
import { afterEach, expect, it } from 'vitest';
import SessionControls from './SessionControls.svelte';
import { buildSessionSearchSchema } from '$lib/searchSchema';
import type { Section } from '../../../routes/sessions/sessions.logic';

let component: ReturnType<typeof mount> | undefined;

afterEach(() => {
	if (component) unmount(component);
	component = undefined;
	document.body.replaceChildren();
});

function setup() {
	component = mount(SessionControls, {
		target: document.body,
		props: {
			rawQuery: '',
			searchSchema: buildSessionSearchSchema(async () => []),
			sections: new Set<Section>(),
			labels: [],
			labelFilter: new Set<string>(),
			cardView: false,
			colorBy: 'none',
			groupBy: 'status',
			onColorBy: () => {},
			onGroupBy: () => {},
			selecting: false,
			searching: false,
			onStartSelect: () => {},
			onCancelSelect: () => {}
		}
	});
	flushSync();
}

function panel() {
	const el = document.querySelector<HTMLElement>('[popover][role="menu"]');
	if (!el) throw new Error('overflow panel did not render');
	return el;
}

// happy-dom implements no Popover API: the panel never fires its own toggle, so
// drive it by hand the way the kit's own consumers' tests do.
async function open() {
	panel().dispatchEvent(Object.assign(new Event('toggle'), { newState: 'open' }));
	await tick();
	await tick();
	flushSync();
}

it('opens the display options through a menu popover', async () => {
	setup();

	const trigger = document.querySelector<HTMLButtonElement>('[data-journey="options"]');
	expect(trigger?.getAttribute('aria-haspopup')).toBe('menu');
	expect(panel().querySelector('[data-journey="display-options"]')).toBeNull();

	await open();

	const menu = panel().querySelector('[data-journey="display-options"]');
	expect(menu).not.toBeNull();
	expect(menu?.querySelectorAll('[data-journey="dimension"]')).toHaveLength(2);
});

it('takes its elevation from the shadow token with no raw fallback', () => {
	setup();
	const style = panel().getAttribute('style') ?? '';
	expect(style).toContain('var(--shadow-lg)');
	expect(style).not.toMatch(/rgba\(/);
});

it('renders the foldable controls inline, not in the menu, above the fold', async () => {
	setup();
	await open();

	// happy-dom ships no ResizeObserver, so the bar keeps its desktop render path.
	expect(document.querySelector('.inline-fold')).not.toBeNull();
	const menu = panel().querySelector('[data-journey="display-options"]');
	expect(menu?.querySelector('[data-journey="view"]')).toBeNull();
	expect(menu?.children).toHaveLength(2);
});
