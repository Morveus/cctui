<script lang="ts">
	import SubagentBadge from '$lib/components/molecules/SubagentBadge.svelte';
	import { m } from '$lib/paraglide/messages';
	import type { SubagentToggle } from './view';

	// Leading slot shared by the checkbox, the subagent toggles and the ↳ child
	// marker.
	let {
		child,
		selectable,
		selected,
		subagentToggles
	}: {
		child: boolean;
		selectable: boolean;
		selected: boolean;
		subagentToggles: SubagentToggle[];
	} = $props();
</script>

{#if selectable}
	<span class="check" class:on={selected} aria-hidden="true">{selected ? '✓' : ''}</span>
{:else}
	<span class="gutter-group">
		{#each subagentToggles as t (t.key)}
			<SubagentBadge
				count={t.count}
				running={t.running}
				open={t.open}
				label={t.label}
				ontoggle={t.ontoggle}
			/>
		{/each}
		{#if child}
			<span class="indent" title={m.sessions_subagent_badge()} aria-hidden="true">↳</span>
		{/if}
	</span>
{/if}

<style>
	.check {
		flex: none;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 1.15rem;
		height: 1.15rem;
		border-radius: var(--r-sm);
		border: 1.5px solid var(--border-strong);
		background: var(--bg);
		color: var(--bg);
		font-size: 0.8rem;
		line-height: 1;
	}
	.check.on {
		background: var(--accent);
		border-color: var(--accent);
		color: var(--bg);
	}
	.gutter-group {
		flex: none;
		display: inline-flex;
		align-items: center;
		gap: var(--sp-1);
	}
	.gutter-group:empty {
		display: none;
	}
	.indent {
		flex: none;
		width: 14px;
		text-align: center;
		line-height: 1;
		font-size: var(--fs-md);
		color: var(--text-faint);
	}
</style>
