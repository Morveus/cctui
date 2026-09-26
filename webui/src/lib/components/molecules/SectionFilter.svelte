<script lang="ts">
	import { Button, Icon } from '@dorsk/tsumikit';
	import { m } from '$lib/paraglide/messages';
	import { clickOutside } from '$lib/clickOutside';
	import { SECTIONS, type Section } from '../../../routes/sessions/sessions.logic';

	// One square toolbar button that opens a popover of four INDEPENDENT on/off
	// section toggles (Starred / Live / Dispatched / Archived) — any combination
	// can be shown at once. `sections` is bindable so the
	// parent owns persistence; toggling mutates it here (never to empty).
	let { sections = $bindable() }: { sections: Set<Section> } = $props();

	let open = $state(false);
	const count = $derived(sections.size);

	function toggle(v: Section) {
		const next = new Set(sections);
		if (next.has(v)) next.delete(v);
		else next.add(v);
		if (next.size === 0) return; // keep at least one on
		sections = next;
	}
</script>

<div class="section-filter" data-journey="sections" use:clickOutside={() => (open = false)}>
	<Button
		square
		data-journey="toggle"
		aria-label={m.sessions_filter_sections()}
		title={m.sessions_filter_sections_count({ count, total: SECTIONS.length })}
		aria-haspopup="true"
		aria-expanded={open}
		aria-pressed={count < SECTIONS.length}
		onclick={() => (open = !open)}
	>
		<Icon name="filter" size={18} />
	</Button>
	{#if count < SECTIONS.length}<span class="count-badge" aria-hidden="true">{count}</span>{/if}
	{#if open}
		<div class="menu" role="menu" aria-label={m.sessions_sections_aria()}>
			{#each SECTIONS as sec (sec.value)}
				<button
					type="button"
					role="menuitemcheckbox"
					class="opt"
					data-journey="option"
					data-journey-key={sec.value}
					aria-checked={sections.has(sec.value)}
					onclick={() => toggle(sec.value)}
				>
					<span class="check" aria-hidden="true">{sections.has(sec.value) ? '✓' : ''}</span>
					<Icon name={sec.icon} size={15} />
					<span class="opt-label">{sec.label}</span>
				</button>
			{/each}
		</div>
	{/if}
</div>

<style>
	.section-filter {
		position: relative;
		display: inline-flex;
		align-items: center;
		flex: none;
	}
	/* Count badge shown when not all sections are enabled (a filter is active). */
	.count-badge {
		position: absolute;
		top: -0.35rem;
		right: -0.35rem;
		min-width: 1rem;
		height: 1rem;
		padding: 0 0.25rem;
		border-radius: 999px;
		background: var(--accent);
		color: var(--bg);
		font-size: 0.62rem;
		font-weight: var(--fw-semibold);
		line-height: 1rem;
		text-align: center;
		pointer-events: none;
	}
	.menu {
		position: absolute;
		top: calc(100% + var(--sp-1));
		right: 0;
		z-index: 40;
		min-width: 12rem;
		display: flex;
		flex-direction: column;
		padding: var(--sp-1);
		border: 1px solid var(--border-strong);
		border-radius: var(--r-md);
		background: var(--bg-elevated);
		box-shadow: var(--shadow-lg);
	}
	.opt {
		display: flex;
		align-items: center;
		gap: var(--sp-2);
		width: 100%;
		padding: var(--sp-2);
		border: none;
		border-radius: var(--r-sm);
		background: none;
		color: var(--text);
		font-size: var(--fs-sm);
		text-align: left;
		cursor: pointer;
	}
	.opt:hover {
		background: var(--bg-hover, var(--border));
	}
	.opt[aria-checked='true'] {
		color: var(--accent, var(--text));
	}
	.check {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 1.05rem;
		height: 1.05rem;
		flex: none;
		border-radius: var(--r-sm);
		border: 1.5px solid var(--border-strong);
		font-size: 0.75rem;
		line-height: 1;
	}
	.opt[aria-checked='true'] .check {
		background: var(--accent);
		border-color: var(--accent);
		color: var(--bg);
	}
	.opt-label {
		flex: 1 1 auto;
	}
</style>
