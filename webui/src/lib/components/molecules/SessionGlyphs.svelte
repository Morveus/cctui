<script lang="ts">
	import type { SessionListItem } from '@bindings/SessionListItem';
	import { machineInitial, machineTint } from '$lib/format';
	import { GlyphStack, type GlyphStackItem } from '@dorsk/tsumikit';
	import { m } from '$lib/paraglide/messages';
	import AccountBadge from './AccountBadge.svelte';
	import MachineBadge from './MachineBadge.svelte';
	import SessionDot from './SessionDot.svelte';

	// Star · status dot · machine · account in one GlyphStack slot (`auto`: folds
	// to 2×2 once the session row is narrow).
	let {
		session,
		livenessClass,
		now,
		stack = 'auto',
		showMachine = true,
		accountWarn = false,
		showAccountName = false,
		onTogglePin,
		onAccountClick
	}: {
		session: SessionListItem;
		livenessClass: string;
		now?: number;
		stack?: 'auto' | 'always' | 'never';
		showMachine?: boolean;
		accountWarn?: boolean;
		showAccountName?: boolean;
		onTogglePin?: (s: SessionListItem) => void;
		onAccountClick?: () => void;
	} = $props();

	const machineLabel = $derived(session.machine_name || session.machine_id.slice(0, 8));

	function pin(e: Event) {
		e.stopPropagation();
		onTogglePin?.(session);
	}

	function holdWhenFolded(e: Event) {
		if ((e.currentTarget as HTMLElement).querySelector('[data-stacked]')) e.stopPropagation();
	}

	const items = $derived<GlyphStackItem[]>([
		...(onTogglePin ? [{ inline: star }] : []),
		{ inline: dot },
		...(showMachine ? [{ inline: machine, stacked: machineTile }] : []),
		{ inline: account }
	]);
</script>

{#snippet star()}
	<span
		class="star"
		class:on={session.pinned}
		role="button"
		tabindex="0"
		title={session.pinned ? m.sessions_unpin_title() : m.sessions_pin_title()}
		aria-pressed={session.pinned}
		aria-label={session.pinned ? m.sessions_unpin_aria() : m.sessions_pin_aria()}
		onpointerdown={(e) => e.stopPropagation()}
		onclick={pin}
		onkeydown={(e) => {
			if (e.key === 'Enter' || e.key === ' ') {
				e.preventDefault();
				pin(e);
			}
		}}>{session.pinned ? '★' : '☆'}</span
	>
{/snippet}
{#snippet dot()}<SessionDot {session} {livenessClass} {now} />{/snippet}
{#snippet machine()}
	<MachineBadge name={session.machine_name} id={session.machine_id} hue={session.machine_hue} mono dense />
{/snippet}
{#snippet machineTile()}
	<span class="mach-tile" style={machineTint(machineLabel, session.machine_hue)} title={machineLabel}
		>{machineInitial(machineLabel)}</span
	>
{/snippet}
{#snippet account()}
	<AccountBadge name={session.account_name} warn={accountWarn} showName={showAccountName} onclick={onAccountClick} />
{/snippet}

<!-- Folded, the stack owns its taps: they open the big tiles instead of the row. -->
<!-- svelte-ignore a11y_no_static_element_interactions, a11y_click_events_have_key_events -->
<span class="glyph-hit" onclick={holdWhenFolded} onpointerdown={holdWhenFolded}>
	<GlyphStack
		{items}
		{stack}
		stackBelow="34rem"
		container={stack === 'auto' ? 'sess-row' : undefined}
		expand="tap"
		expandLabel={m.sessions_glyphs_expand_label()}
	/>
</span>

<style>
	.glyph-hit {
		display: contents;
	}
	.star {
		flex: none;
		min-width: 14px;
		text-align: center;
		line-height: 1;
		font-size: var(--fs-md);
		color: var(--text-faint);
		cursor: pointer;
		user-select: none;
	}
	.star.on,
	.star:hover {
		color: var(--warn);
	}
	.mach-tile {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		min-width: 1rem;
		height: 0.875rem;
		padding: 0 1px;
		border: 1px solid;
		border-radius: var(--r-sm);
		font-family: var(--font-mono);
		font-size: 0.625rem;
		font-weight: 600;
		line-height: 1;
	}
</style>


