<script lang="ts">
	import LabelBadge from '$lib/components/molecules/LabelBadge.svelte';
	import SessionDot from '$lib/components/molecules/SessionDot.svelte';
	import SessionGlyphs from '$lib/components/molecules/SessionGlyphs.svelte';
	import { m } from '$lib/paraglide/messages';
	import { settings } from '$lib/settings.svelte';
	import { Badge, Text, Timestamp } from '@dorsk/tsumikit';
	import { accountTrafficWarning } from '../../../../routes/sessions/sessions.logic';
	import Gutter from './Gutter.svelte';
	import type { SessionActions, SessionView } from './view';

	// gutter · star/dot/machine/account glyphs · title · labels · ⚙N cadence — the lead
	// group both the compact row and the detailed card header open with.
	let {
		view,
		actions,
		row = false
	}: {
		view: SessionView;
		actions: SessionActions;
		/** Compact row: capped title, no activity detail headline. */
		row?: boolean;
	} = $props();

	const s = $derived(view.s);
	const act = $derived(view.act);
	// The in-progress task's activeForm is the more specific version of the
	// daemon's spinner text, so it wins the single bounded headline slot.
	const headline = $derived(act.todoActive ?? act.detail);
</script>

<Gutter
	child={view.child}
	selectable={actions.selectable}
	selected={actions.selected}
	subagentToggles={actions.subagentToggles}
/>
{#if view.child}
	<SessionDot session={s} livenessClass={view.livenessClass} now={view.now} />
	<span class="sub-badge"><Badge tone="info" size="xs">{m.sessions_subagent_badge()}</Badge></span>
{:else}
	<SessionGlyphs
		session={s}
		livenessClass={view.livenessClass}
		now={view.now}
		stack={row ? 'auto' : 'never'}
		showMachine={view.showMachine}
		accountWarn={accountTrafficWarning(s)}
		showAccountName={settings.accountNames}
		onTogglePin={actions.selectable ? undefined : actions.onTogglePin}
	/>
{/if}
<span class="title" class:capped={row}>
	<Text
		data-journey="title"
		weight="semibold"
		size={row ? 'md' : 'lg'}
		truncate
		style="min-width:0;max-width:100%">{view.title}</Text
	>
</span>
{#if s.labels.length > 0 || actions.labelEditable}
	<span class="labels" class:empty={s.labels.length === 0}><LabelBadge
		labels={s.labels}
		editable={actions.labelEditable}
		allLabels={actions.allLabels}
		onCreate={actions.onCreateLabel}
		onAttach={(lid) => actions.onAttachLabel?.(s.id, lid)}
		onDetach={(lid) => actions.onDetachLabel?.(s.id, lid)}
		onUpdate={actions.onUpdateLabel}
		onDelete={actions.onDeleteLabel}
	/></span>
{/if}
{#if act.show && !view.stale}
	<span
		class="activity"
		class:asleep={act.asleep}
		title={act.todoActive ??
			act.detail ??
			(act.asleep ? m.sessions_activity_asleep_title() : m.sessions_activity_live_title())}
	>
		{#if act.count > 0 || act.ageMs !== null}
			<span class="act-cadence"
				>⚙{act.count}{#if act.ageMs !== null && s.last_tool_at}&nbsp;·&nbsp;<Timestamp
						value={s.last_tool_at}
						mode="relative"
						size="xs"
						tone="inherit"
					/>{/if}</span
			>
		{/if}
		{#if act.todoTotal > 0}
			<span class="act-todos" class:running={act.todoActive !== null}
				>{act.todoDone}/{act.todoTotal}</span
			>
		{/if}
		{#if headline && !row}<span class="act-detail">{headline}</span>{/if}
	</span>
{/if}

<style>
	.labels,
	.sub-badge {
		display: contents;
	}
	/* Em, not rem: at a large text scale the row runs out of room well before
	   its pixel width says so. */
	@container sess-row (max-width: 20em) {
		.labels.empty,
		.sub-badge {
			display: none;
		}
	}
	.title {
		display: inline-flex;
		flex: 0 1 auto;
		min-width: 0;
	}
	/* `ch` has to resolve against the title's own size, not the row's. */
	.title.capped {
		font-size: var(--fs-md);
		max-width: min(28ch, 40%);
	}
	/* The surrounding chips have degraded by here, so the title takes the slack
	   instead of being the first thing squeezed: a capped, shrinkable title goes
	   to zero width on a phone, which is both unreadable and untappable. */
	@container sess-row (max-width: 34rem) {
		.title.capped {
			flex: 1 1 auto;
			max-width: none;
			min-width: 6ch;
		}
	}
	.activity {
		display: inline-flex;
		align-items: baseline;
		gap: var(--sp-1);
		min-width: 0;
		flex: 0 1 auto;
		overflow: hidden;
		font-size: var(--fs-xs);
		color: var(--text-faint);
		white-space: nowrap;
	}
	@container sess-row (max-width: 40rem) {
		.activity {
			display: none;
		}
	}
	.act-cadence {
		flex: none;
		font-variant-numeric: tabular-nums;
	}
	/* Faint = every task pending/done, accent = one is in_progress, so the row
	   distinguishes "has a list" from "actively working a step" at a glance. */
	.act-todos {
		flex: none;
		font-variant-numeric: tabular-nums;
	}
	.act-todos.running {
		color: var(--accent);
		font-weight: 600;
	}
	/* `act-detail` carries arbitrary-length agent prose (the in_progress
	   activeForm). It must stay capped and ellipsized: an unbounded string here
	   grows the lead row and wraps the session emoji onto a second line. */
	.act-detail {
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		color: var(--text-muted);
		max-width: 22rem;
	}
	.activity.asleep,
	.activity.asleep .act-detail {
		color: var(--warn);
	}
</style>
