<script lang="ts">
	// Settings › Resource monitoring: every machine this instance has a daemon
	// on, one checkbox each. A ticked machine gets a CPU / memory / disk gauge
	// in the header strip (ResourceBattery). The list is the same query the
	// strip reads, so the figures shown here are the live ones. Each machine also
	// carries its RAM ceiling for spawns (MemCeilingField).
	import { Checkbox, Text } from '@dorsk/tsumikit';
	import SettingGroup from '$lib/components/molecules/SettingGroup.svelte';
	import SettingRow from '$lib/components/molecules/SettingRow.svelte';
	import SettingSection from '$lib/components/molecules/SettingSection.svelte';
	import MemCeilingField from './MemCeilingField.svelte';
	import { useMachineResources } from '$lib/queries';
	import { settings } from '$lib/settings.svelte';
	import { m } from '$lib/paraglide/messages';
	import { machineLabel, pctOf } from '$lib/components/molecules/resource-gauge.logic';

	const q = useMachineResources(() => true);
	const rows = $derived(q.data ?? []);
	const pinned = $derived(settings.monitoredMachines);
	const pctText = (v: number | null | undefined) => {
		const p = pctOf(v);
		return p === null ? '?' : `${p}%`;
	};
	function summary(r: (typeof rows)[number]): string {
		if (!r.resources) return m.resource_gauge_unknown();
		return `${m.resource_gauge_cpu()} ${pctText(r.resources.cpu_pct)} · ${m.resource_gauge_mem()} ${pctText(r.resources.mem_pct)} · ${m.resource_gauge_disk()} ${pctText(r.resources.disk_pct)}`;
	}
</script>

<SettingSection id="monitoring" icon="▥" title={m.settings_monitoring_title()}>
	<SettingGroup>
		<SettingRow
			label={m.settings_monitoring_machines_label()}
			help={m.settings_monitoring_machines_help()}
			wide
			selfLabelled
		>
			{#if q.isPending}
				<Text size="sm" tone="faint">{m.settings_monitoring_loading()}</Text>
			{:else if q.isError}
				<Text size="sm" tone="danger">{m.settings_monitoring_error()}</Text>
			{:else if rows.length === 0}
				<Text size="sm" tone="faint">{m.settings_monitoring_empty()}</Text>
			{:else}
				<ul class="list">
					{#each rows as r (r.machine_id)}
						<li class="item" data-setting-row>
							<Checkbox
								label={machineLabel(r)}
								checked={pinned.includes(r.machine_id)}
								onchange={(e) =>
									settings.setMonitoredMachine(
										r.machine_id,
										(e.currentTarget as HTMLInputElement).checked
									)}
							/>
							<span class="meta">
								<Text size="xs" tone="faint" variant="code">{r.name}</Text>
								<Text size="xs" tone={r.liveness === 'online' ? 'faint' : 'danger'}
									>{r.liveness}</Text
								>
								<Text size="xs" tone="faint">{summary(r)}</Text>
							</span>
							<MemCeilingField row={r} />
						</li>
					{/each}
				</ul>
				<div class="ceiling-help"><Text size="xs" tone="faint">{m.settings_mem_ceiling_help()}</Text></div>
			{/if}
		</SettingRow>
	</SettingGroup>
</SettingSection>

<style>
	.list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: var(--sp-2);
	}
	.item {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--sp-1) var(--sp-3);
	}
	.ceiling-help {
		margin: var(--sp-2) 0 0;
	}
	.meta {
		display: inline-flex;
		flex-wrap: wrap;
		gap: var(--sp-2);
		padding-left: var(--sp-4);
	}
</style>
