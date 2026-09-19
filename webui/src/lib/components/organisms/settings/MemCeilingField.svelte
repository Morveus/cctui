<script lang="ts">
	// One machine's RAM ceiling for spawns, in GiB. Empty = no ceiling
	// ("Unlimited"). Saved on Enter or when the field loses focus with a changed
	// value; the server refuses less than one session's estimate (1.5 GiB), and
	// so does the field before it asks.
	import { Input, Text } from '@dorsk/tsumikit';
	import { useQueryClient } from '@tanstack/svelte-query';
	import type { MachineResourcesRow } from '@bindings/MachineResourcesRow';
	import { errMessage } from '$lib/api';
	import { endpoints, qk } from '$lib/queries';
	import { m } from '$lib/paraglide/messages';
	import { toasts } from '$lib/toast.svelte';
	import { ceilingInputValue, fmtGiB, parseCeilingGiB } from '$lib/memCeiling';
	import { machineLabel } from '$lib/components/molecules/resource-gauge.logic';

	let { row }: { row: MachineResourcesRow } = $props();

	const qc = useQueryClient();
	const saved = $derived(ceilingInputValue(row.mem_ceiling_bytes));
	// The field follows the server value until the user types into it.
	let draft = $state<string | null>(null);
	const value = $derived(draft ?? saved);
	let saving = $state(false);

	const parsed = $derived(parseCeilingGiB(value));
	const error = $derived(
		parsed.ok
			? null
			: parsed.reason === 'too_small'
				? m.settings_mem_ceiling_too_small()
				: m.settings_mem_ceiling_invalid()
	);
	const total = $derived(row.resources?.mem_total_bytes ?? 0);

	async function save() {
		if (draft === null || saving) return;
		if (!parsed.ok) return;
		if (parsed.bytes === row.mem_ceiling_bytes) {
			draft = null;
			return;
		}
		saving = true;
		try {
			await endpoints.setMemCeiling(row.machine_id, parsed.bytes);
			qc.setQueryData<MachineResourcesRow[]>(qk.machineResources, (old) =>
				old?.map((r) =>
					r.machine_id === row.machine_id ? { ...r, mem_ceiling_bytes: parsed.bytes } : r
				)
			);
			draft = null;
			toasts.ok(
				parsed.bytes === null
					? m.settings_mem_ceiling_cleared({ machine: machineLabel(row) })
					: m.settings_mem_ceiling_saved({
							machine: machineLabel(row),
							ceiling: fmtGiB(parsed.bytes)
						})
			);
		} catch (e) {
			toasts.error(m.settings_mem_ceiling_save_failed({ error: errMessage(e) }));
		} finally {
			saving = false;
		}
	}
</script>

<span class="ceiling">
	<Text size="xs" tone="faint">{m.settings_mem_ceiling_label()}</Text>
	<Input
		size="sm"
		width="6rem"
		inputmode="decimal"
		placeholder={m.settings_mem_ceiling_unlimited()}
		aria-label={m.settings_mem_ceiling_aria({ machine: machineLabel(row) })}
		invalid={!!error}
		disabled={saving}
		{value}
		oninput={(e) => (draft = (e.currentTarget as HTMLInputElement).value)}
		onenter={() => void save()}
		onblur={() => void save()}
	/>
	<Text size="xs" tone="faint">{m.settings_mem_ceiling_unit()}</Text>
	{#if total > 0}
		<Text size="xs" tone="faint">{m.settings_mem_ceiling_total({ total: fmtGiB(total) })}</Text>
	{/if}
	{#if error}
		<Text size="xs" tone="danger">{error}</Text>
	{/if}
</span>

<style>
	.ceiling {
		display: inline-flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--sp-1) var(--sp-2);
		padding-left: var(--sp-4);
	}
</style>
