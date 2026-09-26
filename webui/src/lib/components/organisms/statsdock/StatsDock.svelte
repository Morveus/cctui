<script lang="ts">
	import { DOCK_MIN_PX, maxDockWidth, type DockSide } from '$lib/dock';
	import { settings } from '$lib/settings.svelte';
	import { useVersion } from '$lib/queries';
	import NavLink from '$lib/components/atoms/NavLink.svelte';
	import UpdateModal from '$lib/components/organisms/UpdateModal.svelte';
	import { Button, Text } from '@dorsk/tsumikit';
	import { resizeHandle } from '@dorsk/tsumikit';
	import AccountUsageList from './AccountUsageList.svelte';
	import PoolUsageList from './PoolUsageList.svelte';
	import TokenWindows from './TokenWindows.svelte';
	import OverviewTiles from './OverviewTiles.svelte';
	import UsageCharts from './UsageCharts.svelte';
	import WindowHistory from './WindowHistory.svelte';
	import { m } from '$lib/paraglide/messages';

	// The stats panel pinned to one edge of the Sessions screen (Settings ›
	// Stats panel): account usage gauges, rolling token windows, the Overview
	// counts and its charts, each in a foldable section. When `stacked`, the
	// spawn form owns the top half of the same column and this panel takes the
	// bottom half at the spawn panel's width. `width` is whatever the layout
	// reserved on this edge (resolveDocks), so the two never drift apart; the
	// grip on the inner edge writes a new width back to the settings, and a
	// stacked column resizes through the spawn panel's width.
	let {
		side,
		stacked = false,
		width
	}: { side: DockSide; stacked?: boolean; width: string } = $props();

	function setWidth(px: number | undefined) {
		if (stacked) settings.setSpawnDock({ width: px });
		else settings.setStatsDock({ width: px });
	}
	let dragging = $state(false);
	let viewportWidth = $state(0);
	const maxPx = $derived(maxDockWidth(viewportWidth));

	// Server + client versions, with the red ↑ chip when the server's release
	// probe found something newer. They used to live in the header; the redesign
	// gave that room away, and this panel is the one piece of always-on chrome
	// left where a build number belongs.
	const version = useVersion();
	let updateOpen = $state(false);

	const sections = [
		{ key: 'pools', title: () => m.stats_dock_pools(), open: true },
		{ key: 'accounts', title: () => m.stats_dock_accounts(), open: true },
		{ key: 'tokens', title: () => m.home_token_usage(), open: true },
		{ key: 'overview', title: () => m.home_overview_title(), open: true },
		{ key: 'history', title: () => m.stats_dock_window_history(), open: false },
		{ key: 'usage', title: () => m.home_usage_title(), open: false }
	];
</script>

<svelte:window bind:innerWidth={viewportWidth} />

<aside
	class="dock"
	class:dock-left={side === 'left'}
	class:stacked
	style:--stats-dock-w={width}
	aria-label={m.stats_dock_title()}
>
	<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
	<div
		class="grip"
		class:grip-left={side === 'left'}
		class:dragging
		role="separator"
		tabindex="0"
		aria-orientation="vertical"
		aria-valuemin={DOCK_MIN_PX}
		aria-valuemax={maxPx}
		aria-label={m.dock_resize_grip()}
		title={m.dock_resize_grip()}
		use:resizeHandle={{
			side: side,
			min: DOCK_MIN_PX,
			max: maxPx,
			onwidth: setWidth,
			onreset: () => setWidth(undefined),
			onactive: (a) => {
				dragging = a;
				document.body.classList.toggle('dock-resizing', a);
			}
		}}
	></div>
	<div class="dock-head">{m.stats_dock_title()}</div>
	<div class="dock-body">
		{#each sections as s (s.key)}
			<details class="section" open={s.open}>
				<summary>{s.title()}</summary>
				<div class="section-body">
					{#if s.key === 'pools'}
						<PoolUsageList />
					{:else if s.key === 'accounts'}
						<AccountUsageList />
					{:else if s.key === 'tokens'}
						<TokenWindows />
					{:else if s.key === 'overview'}
						<OverviewTiles />
					{:else if s.key === 'history'}
						<WindowHistory />
					{:else}
						<UsageCharts />
					{/if}
				</div>
			</details>
		{/each}
	</div>
	{#if version.data}
		<div class="dock-ver">
			<NavLink href={version.data.commit_url} target="_blank" rel="noopener">
				<Text size="xs" tone="faint" variant="code">srv v{version.data.version}</Text>
			</NavLink>
			<Text size="xs" tone="faint" variant="code">ui v{__CLIENT_VERSION__}</Text>
			{#if version.data.latest_version}
				<!-- NOT tsumikit's `chip`: that is a fixed 2.5rem square with padding 0
				     meant for a lone glyph, and the version text spills out of it.
				     A plain ghost button sized to its content instead. -->
				<Button
					size="sm"
					variant="ghost"
					style="height: 22px; min-height: 22px; width: auto; min-width: 0; padding: 0 var(--sp-1); flex: none;"
					title={m.nav_update_available({ version: version.data.latest_version })}
					aria-label={m.nav_update_available({ version: version.data.latest_version })}
					onclick={() => (updateOpen = true)}
				>
					<Text size="xs" variant="code" tone="danger">↑ v{version.data.latest_version}</Text>
				</Button>
			{/if}
		</div>
	{/if}
</aside>

{#if updateOpen && version.data?.latest_version}
	<UpdateModal
		latestVersion={version.data.latest_version}
		latestUrl={version.data.latest_url ?? version.data.repo_url}
		selfUpdateReady={version.data.self_update_ready}
		selfUpdateHook={version.data.self_update_hook}
		onclose={() => (updateOpen = false)}
	/>
{/if}

<style>
	.dock {
		position: fixed;
		top: calc(var(--header-h) + var(--safe-top));
		bottom: var(--bottom-chrome, calc(var(--nav-h) + var(--safe-bottom)));
		right: 0;
		width: var(--stats-dock-w);
		display: flex;
		flex-direction: column;
		background: var(--bg-elevated);
		border-left: 1px solid var(--border);
		z-index: 4;
	}
	.dock.dock-left {
		right: auto;
		left: 0;
		border-left: 0;
		border-right: 1px solid var(--border);
	}
	/* Sharing the column with the spawn form: bottom half only. */
	.dock.stacked {
		top: 50%;
		border-top: 1px solid var(--border);
	}
	/* A 10px hit area straddling the panel's border, with a 2px line that only
	   shows on hover, focus or while dragging so the border stays quiet otherwise. */
	.grip {
		position: absolute;
		top: 0;
		bottom: 0;
		left: -5px;
		width: 10px;
		cursor: ew-resize;
		touch-action: none;
		z-index: 1;
	}
	.grip-left {
		left: auto;
		right: -5px;
	}
	/* An always-visible knob in the middle of the edge, like the kit's panel handle. */
	.grip::before {
		content: '';
		position: absolute;
		top: 50%;
		left: 3px;
		width: 4px;
		height: 2.5rem;
		margin-top: -1.25rem;
		border-radius: var(--r-pill);
		background: var(--border-strong);
	}
	.grip:hover::before,
	.grip.dragging::before {
		background: var(--accent);
	}
	.grip::after {
		content: '';
		position: absolute;
		top: 0;
		bottom: 0;
		left: 4px;
		width: 2px;
		background: var(--accent);
		opacity: 0;
		transition: opacity 0.12s var(--ease);
	}
	.grip:hover::after,
	.grip:focus-visible::after,
	.grip.dragging::after {
		opacity: 1;
	}
	.grip:focus-visible {
		outline: none;
	}
	@media (hover: none) {
		.grip::after {
			opacity: 0.35;
		}
	}
	.dock-head {
		flex: none;
		padding: var(--sp-3) var(--sp-3) var(--sp-2);
		font-weight: var(--fw-semibold);
		border-bottom: 1px solid var(--border);
	}
	.dock-body {
		flex: 1;
		min-height: 0;
		overflow-y: auto;
		padding: var(--sp-2) var(--sp-3) var(--sp-3);
		display: flex;
		flex-direction: column;
		gap: var(--sp-1);
	}
	.section {
		border-bottom: 1px solid var(--border);
		padding-bottom: var(--sp-2);
	}
	.section:last-child {
		border-bottom: 0;
	}
	.section summary {
		cursor: pointer;
		padding: var(--sp-2) 0;
		font-size: var(--fs-xs);
		font-weight: var(--fw-semibold);
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--text-muted);
		user-select: none;
	}
	.section-body {
		padding-top: var(--sp-1);
	}
	/* Version strip pinned under the scrolling body, mirroring .dock-head. */
	.dock-ver {
		flex: none;
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--sp-1) var(--sp-2);
		padding: var(--sp-2) var(--sp-3);
		border-top: 1px solid var(--border);
	}
</style>
