<script lang="ts">
	import { useAllAccountsUsage, useUsageCloses, useUsageHistory } from '$lib/queries';
	import { Field, Select, Text, Timestamp } from '@dorsk/tsumikit';
	import SegmentedControl from '$lib/components/molecules/SegmentedControl.svelte';
	import { m } from '$lib/paraglide/messages';
	import { LOOKBACK_MS, polyline, windowLines } from './window-history';

	// Utilization across each past instance of one quota window, one line per
	// instance on a shared elapsed-time axis, plus how the latest ones closed.
	const accounts = useAllAccountsUsage();
	const rows = $derived(accounts.data ?? []);
	let picked = $state<string | null>(null);
	const accountId = $derived(picked ?? rows[0]?.account_id ?? null);
	let windowKey = $state('session');

	const mountedAt = Date.now();
	const from = $derived(new Date(mountedAt - LOOKBACK_MS[windowKey]).toISOString());
	const history = useUsageHistory(
		() => accountId,
		() => windowKey,
		() => from
	);
	const closes = useUsageCloses(() => from);

	const lines = $derived(windowLines(history.data?.samples ?? [], windowKey));
	const recentCloses = $derived(
		(closes.data?.closes ?? [])
			.filter((c) => c.account_id === accountId && c.window_key === windowKey)
			.slice(0, 5)
	);
	const windowOptions = [
		{ value: 'session', label: m.stats_dock_window_5h() },
		{ value: 'weekly_all', label: m.stats_dock_window_weekly() }
	];
</script>

{#if rows.length === 0}
	<Text tone="faint" size="sm">{m.stats_dock_no_accounts()}</Text>
{:else}
	<div class="history">
		<Field label={m.stats_dock_window_history_account()}>
			<Select
				value={accountId ?? ''}
				onchange={(e) => (picked = (e.currentTarget as HTMLSelectElement).value)}
			>
				{#each rows as r (r.account_id)}
					<option value={r.account_id}>{r.account_name} · {r.provider}</option>
				{/each}
			</Select>
		</Field>
		<SegmentedControl
			value={windowKey}
			options={windowOptions}
			label={m.stats_dock_window_history()}
			onchange={(v) => (windowKey = v)}
		/>
		{#if history.isLoading}
			<Text tone="faint" size="sm">{m.common_loading()}</Text>
		{:else if lines.length === 0}
			<Text tone="faint" size="sm">{m.stats_dock_window_history_empty()}</Text>
		{:else}
			<svg
				class="chart"
				viewBox="0 0 100 100"
				preserveAspectRatio="none"
				role="img"
				aria-label={m.stats_dock_window_history()}
			>
				<line class="grid" x1="0" y1="50" x2="100" y2="50" />
				{#each lines as line, i (line.resetsAt)}
					<polyline class:current={i === lines.length - 1} points={polyline(line)} />
				{/each}
			</svg>
		{/if}
		{#each recentCloses as c (c.resets_at)}
			<div class="close">
				<Text size="xs" tone="muted"><Timestamp value={c.resets_at} mode="relative" tone="inherit" /></Text>
				<Text size="xs" numeric>
					{m.stats_dock_window_close({
						used: Math.round(c.final_utilization),
						wasted: Math.round(c.wasted_pct)
					})}
				</Text>
			</div>
		{/each}
	</div>
{/if}

<style>
	.history {
		display: flex;
		flex-direction: column;
		gap: var(--sp-2);
	}
	.chart {
		width: 100%;
		height: 6rem;
		border: 1px solid var(--border);
		border-radius: var(--r-sm);
	}
	.chart polyline {
		fill: none;
		stroke: var(--text-faint);
		stroke-width: 1;
		vector-effect: non-scaling-stroke;
		opacity: 0.6;
	}
	.chart polyline.current {
		stroke: var(--accent);
		stroke-width: 2;
		opacity: 1;
	}
	.grid {
		stroke: var(--border);
		stroke-dasharray: 2 2;
		vector-effect: non-scaling-stroke;
	}
	.close {
		display: flex;
		justify-content: space-between;
		gap: var(--sp-2);
	}
</style>
