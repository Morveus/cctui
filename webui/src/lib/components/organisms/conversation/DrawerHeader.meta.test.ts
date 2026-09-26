import { describe, expect, it } from 'vitest';
import header from './DrawerHeader.svelte?raw';
import en from '../../../../../messages/en.json?raw';
import fr from '../../../../../messages/fr.json?raw';

const markup = header.slice(header.indexOf('</script>'));
const script = header.slice(0, header.indexOf('</script>'));
const css = header.slice(header.indexOf('<style>'));
const trail = markup.slice(markup.indexOf('<div class="meta-trail">'), markup.indexOf('{#if keepaliveOpen}'));
const popover = markup.slice(markup.indexOf('<Popover'), markup.indexOf('</Popover>'));

describe('drawer header meta row', () => {
	it('keeps cwd and branch on the row at every width', () => {
		expect(markup).toContain('<WorkingDir');
		expect(markup).toContain('m.sessions_branch_title({ branch })');
		for (const s of ['<WorkingDir', 'class="branch"']) expect(popover, s).not.toContain(s);
	});

	it('decides every width from the drawer container, never the viewport or JS', () => {
		expect(css).toContain('container: drawer-head / inline-size');
		expect(css).toContain('@container drawer-head (max-width: 40rem)');
		expect(css).toContain('@container drawer-head (max-width: 26rem)');
		expect(css).not.toContain('@media');
		expect(header).not.toContain('clientWidth={headWidth}');
		expect(script).not.toContain('getComputedStyle');
	});

	it('drops langfuse below 40rem and the model badge and its editor below 26rem', () => {
		const wide = css.slice(css.indexOf('@container drawer-head (max-width: 40rem)'));
		expect(wide.slice(0, wide.indexOf('\n\t}'))).toContain('.langfuse,');
		const narrow = css.slice(css.indexOf('@container drawer-head (max-width: 26rem)'));
		const body = narrow.slice(0, narrow.indexOf('\n\t}'));
		expect(body).toContain('.model,');
		expect(body).toContain('.model-edit');
	});

	it('declares the model-edit base rule before the query that hides it', () => {
		expect(css.indexOf('.model-edit {')).toBeLessThan(css.indexOf('@container drawer-head (max-width: 26rem)'));
	});

	it('reveals the ⓘ trigger exactly when the first item goes', () => {
		expect(css).toMatch(/\n\t\.meta-details \{\n\t\tdisplay: none;/);
		const wide = css.slice(css.indexOf('@container drawer-head (max-width: 40rem)'));
		expect(wide.slice(0, wide.indexOf('\n\t}'))).toContain('.meta-details {');
		expect(trail).toContain('{#if hasModelMeta}');
		expect(script).toContain('const hasModelMeta = $derived(');
	});

	it('puts the droppable items behind the trigger, model editor included', () => {
		expect(popover).toContain('<span class="langfuse"><LangfuseChip');
		expect(popover).toContain("{@render modelMeta('drawer-details')}");
		const snippet = markup.slice(markup.indexOf('{#snippet modelMeta'), markup.indexOf('{#snippet modelMeta') + 2000);
		expect(snippet).toContain('<ModelPicker');
		expect(snippet).toContain('onclick={applyModelChange}');
		expect(snippet).toContain('onclick={() => (modelEditing = false)}');
		expect(snippet).toContain('onclick={openModelEditor}');
	});

	it('gives the two model editor instances distinct ids', () => {
		expect(markup).toContain('{#snippet modelMeta(idPrefix: string)}');
		expect(markup).toContain('id="{idPrefix}-model"');
		expect(markup).not.toContain('id="drawer-model"');
	});

	it('names the trigger and lets the kit own aria-expanded and Escape', () => {
		expect(popover).toContain('label={m.drawer_meta_details()}');
		expect(popover).toContain('{#snippet trigger()}<Icon name="info"');
		expect(header).toContain('Popover,');
		for (const msgs of [en, fr]) expect(JSON.parse(msgs).drawer_meta_details).toBeTruthy();
	});

	it('un-hides everything the row dropped once it is inside the popover', () => {
		expect(css).toContain('.metapop .m-full');
		expect(css).toContain('.metapop .m-short');
		expect(css).toContain('.metapop .langfuse,');
		expect(css).toContain('.metapop .model-edit');
		expect(css).toContain('.metapop .model {');
	});

	it('adds no horizontal overflow escape hatch', () => {
		expect(header).not.toContain(':global(');
		expect(header).not.toContain('overflow-x');
	});
});
