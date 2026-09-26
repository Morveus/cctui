<script lang="ts">
	import { useAllMachines, useSessionStats, useUsageCloses } from '$lib/queries';
	import { m } from '$lib/paraglide/messages';
	import { getLocale } from '$lib/paraglide/runtime';
	import MetricTile from '$lib/components/molecules/MetricTile.svelte';
	import { buildMetricTiles, type MetricKey } from '../../../../routes/home.logic';
	import { wastedPct } from './window-history';

	const stats = useSessionStats();
	const machines = useAllMachines(() => true);

	const num = (n: number) => n.toLocaleString(getLocale());
	const tiles = $derived(buildMetricTiles(stats.data, machines.data ?? []));
	const weekAgo = new Date(Date.now() - 7 * 24 * 3600_000).toISOString();
	const closes = useUsageCloses(() => weekAgo);
	const wasted5h = $derived(wastedPct(closes.data?.summary, 'session'));
	const wastedWeekly = $derived(wastedPct(closes.data?.summary, 'weekly_all'));
	const pctOrDash = (v: number | null) => (v === null ? '–' : `${v}%`);
	const label = (key: MetricKey) =>
		key === 'live'
			? m.home_stat_live()
			: key === 'needs_input'
				? m.home_stat_needs_input()
				: key === 'machines'
					? m.home_stat_machines_online()
					: m.home_stat_total_sessions();
</script>

<div class="tiles">
	{#each tiles as t (t.key)}
		<MetricTile compact value={num(t.value)} suffix={t.suffix} warn={t.warn} label={label(t.key)} />
	{/each}
	{#if wasted5h !== null || wastedWeekly !== null}
		<div class="wide">
			<MetricTile
				compact
				value={`${pctOrDash(wasted5h)} · ${pctOrDash(wastedWeekly)}`}
				label={m.stats_dock_wasted_last_week()}
			/>
		</div>
	{/if}
</div>

<style>
	.tiles {
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
		gap: var(--sp-2);
	}
	.wide {
		grid-column: 1 / -1;
	}
</style>
