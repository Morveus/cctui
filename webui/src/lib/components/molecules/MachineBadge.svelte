<script lang="ts">
	import { machineInitial, machineTint } from '$lib/format';
	import { Badge } from '@dorsk/tsumikit';

	// Deterministic-color machine badge, shared by the session list and the
	// chat header so the same machine reads the same hue everywhere. Composes the
	// base Badge and overrides its palette with a per-machine hue.
	let {
		name,
		id,
		hue,
		mono = false,
		dense = false
	}: {
		name?: string | null;
		id: string;
		/** Operator-set hue override; falls back to the name hash. */
		hue?: number | null;
		mono?: boolean;
		/** Collapse to the initial once the row is too narrow to seat the name.
		 *  Only the session row and card name a container to measure against. */
		dense?: boolean;
	} = $props();

	const label = $derived(name || id.slice(0, 8));
	const tint = $derived(machineTint(label, hue));
</script>

<Badge class={mono ? 'mono' : ''} style={tint} title={dense ? label : undefined}>
	{#if dense}
		<span class="full">{label}</span><span class="initial">{machineInitial(label)}</span>
	{:else}
		{label}
	{/if}
</Badge>

<style>
	.initial {
		display: none;
	}
	@container sess-row (max-width: 34rem) {
		.full {
			display: none;
		}
		.initial {
			display: inline;
		}
	}
	@container sess-card (max-width: 18rem) {
		.full {
			display: none;
		}
		.initial {
			display: inline;
		}
	}
</style>
