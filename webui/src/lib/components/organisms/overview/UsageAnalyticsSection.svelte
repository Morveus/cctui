<script lang="ts">
	import { useUsageAnalytics } from '$lib/queries';
	import { m } from '$lib/paraglide/messages';
	import { Card, Cluster, Stack, Text } from '@dorsk/tsumikit';
	import TokensOverTime from './TokensOverTime.svelte';
	import ModelBreakdown from './ModelBreakdown.svelte';
	import ActivityHeatmap from './ActivityHeatmap.svelte';
	import CacheLossCard from './CacheLossCard.svelte';
	import { RANGES, hasUsage, type Granularity } from './usage-analytics';

	let { rangeKey = '30d' }: { rangeKey?: string } = $props();

	const range = $derived(RANGES.find((r) => r.key === rangeKey) ?? RANGES[2]);
	const q = useUsageAnalytics(() => range.days);
	const data = $derived(q.data);
	const show = $derived(hasUsage(data));
</script>

{#if q.isLoading}
	<Card><Text tone="faint">{m.common_loading()}</Text></Card>
{:else if !show}
	<Card><Text tone="faint">{m.home_usage_no_data()}</Text></Card>
{:else if data}
	<Stack gap="var(--sp-3)">
		<div class="charts">
			<div class="split">
				<Card>
					<Stack gap="var(--sp-3)">
						<Text size="sm" weight="semibold">{m.home_usage_tokens_per_day()}</Text>
						<TokensOverTime
							buckets={data.buckets}
							days={range.days}
							granularity={data.granularity as Granularity}
						/>
					</Stack>
				</Card>
				<Card>
					<Stack gap="var(--sp-3)">
						<Text size="sm" weight="semibold">{m.home_usage_models_output()}</Text>
						{#if data.models.length}
							<ModelBreakdown models={data.models} />
						{:else}
							<Text tone="faint" size="sm">{m.home_usage_no_data()}</Text>
						{/if}
					</Stack>
				</Card>
			</div>
		</div>
		<Card>
			<Stack gap="var(--sp-3)">
				<Cluster gap="var(--sp-3)" align="baseline">
					<Text size="sm" weight="semibold">{m.home_usage_heatmap()}</Text>
					<Text size="xs" tone="faint">{m.home_usage_heatmap_caption()}</Text>
				</Cluster>
				<ActivityHeatmap cells={data.heatmap} />
			</Stack>
		</Card>
		<CacheLossCard />
	</Stack>
{/if}

<style>
	.charts {
		container-type: inline-size;
	}
	.split {
		display: grid;
		grid-template-columns: minmax(0, 1.4fr) minmax(0, 1fr);
		gap: var(--sp-3);
	}
	@container (max-width: 48rem) {
		.split {
			grid-template-columns: minmax(0, 1fr);
		}
	}
</style>
