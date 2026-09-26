<script lang="ts">
	// `#` session-mention popover. Wraps a textarea (passed as children) and
	// watches its caret: typing `#` opens a list of non-archived sessions (see
	// $lib/mention), filtered by what follows the `#`. Up/Down move
	// the highlight, Enter/Tab or a tap picks, Escape closes. Picking replaces
	// the `#query` with `#<id> (<name>) ` so the user can hand the id to their
	// agent. The panel spans the wrapper's width: `placement="up"` pins it above
	// the field (the chat composer sits at the bottom of the viewport), `auto`
	// opens below and flips up when the viewport has no room.
	import type { SessionListItem } from '@bindings/SessionListItem';
	import type { Snippet } from 'svelte';
	import { tick, untrack } from 'svelte';
	import { Text } from '@dorsk/tsumikit';
	import { clickOutside } from '$lib/clickOutside';
	import { m } from '$lib/paraglide/messages';
	import {
		applyMention,
		filterMentions,
		findTrigger,
		mentionableSessions,
		moveSelection,
		type MentionTrigger
	} from '$lib/mention/mention';

	let {
		value = $bindable(''),
		el = null,
		sessions,
		excludeId = null,
		placement = 'auto',
		children
	}: {
		value: string;
		/** The wrapped textarea element (from the Textarea atom's `bind:el`). */
		el?: HTMLTextAreaElement | null;
		/** Full session list; filtered to still-working ones here. */
		sessions: SessionListItem[];
		/** Session the field belongs to, never offered to itself. */
		excludeId?: string | null;
		placement?: 'up' | 'auto';
		children: Snippet;
	} = $props();

	let trigger = $state<MentionTrigger | null>(null);
	let index = $state(0);
	let flipUp = $state(false);
	let listEl = $state<HTMLElement | null>(null);
	// `#` position the user dismissed (Escape / click away): the panel stays
	// closed while the caret sits in that same trigger and comes back once the
	// caret leaves it, or focus returns to the field.
	let dismissed = $state<number | null>(null);

	const candidates = $derived(mentionableSessions(sessions, excludeId));
	const matches = $derived(trigger ? filterMentions(candidates, trigger.query).slice(0, 30) : []);
	const open = $derived(trigger !== null);
	const up = $derived(placement === 'up' || flipUp);

	// Re-read the field after any edit, caret move or focus change. Reads the
	// DOM value rather than the bound prop: the binding may lag the input event.
	function refresh() {
		if (!el) return;
		const text = el.value;
		const next = candidates.length ? findTrigger(text, el.selectionStart ?? text.length) : null;
		if (!next) dismissed = null;
		if (next && dismissed === next.start) {
			trigger = null;
			return;
		}
		const wasOpen = trigger !== null;
		trigger = next;
		if (!wasOpen && next) {
			index = 0;
			if (placement === 'auto') {
				const r = el.getBoundingClientRect();
				flipUp = window.innerHeight - r.bottom < 280;
			}
		}
	}
	// Programmatic value changes (send clears the draft, history recall) must
	// re-evaluate too, not only the field's own input events.
	$effect(() => {
		void value;
		void candidates;
		untrack(refresh);
	});
	$effect(() => {
		if (index >= matches.length) index = 0;
	});
	$effect(() => {
		// Keep the highlighted row in view while arrowing through a long list.
		const row = listEl?.children[index] as HTMLElement | undefined;
		row?.scrollIntoView?.({ block: 'nearest' });
	});

	function close() {
		trigger = null;
	}
	function dismiss() {
		if (trigger) dismissed = trigger.start;
		close();
	}

	async function pick(s: SessionListItem) {
		if (!el || !trigger) return;
		const caret = el.selectionStart ?? el.value.length;
		const out = applyMention(el.value, caret, trigger, s);
		value = out.text;
		close();
		await tick();
		el.focus();
		el.setSelectionRange(out.caret, out.caret);
	}

	// Capture-phase so the wrapped field's own Enter/Arrow handlers (send,
	// history recall) never see the keys the panel consumes.
	function onKeydownCapture(e: KeyboardEvent) {
		if (!open) return;
		switch (e.key) {
			case 'ArrowDown':
			case 'ArrowUp':
				e.preventDefault();
				e.stopPropagation();
				index = moveSelection(index, e.key === 'ArrowDown' ? 1 : -1, matches.length);
				return;
			case 'Enter':
			case 'Tab':
				if (e.shiftKey || e.ctrlKey || e.metaKey || e.altKey) return;
				if (!matches.length) return;
				e.preventDefault();
				e.stopPropagation();
				void pick(matches[index]);
				return;
			case 'Escape':
				e.preventDefault();
				e.stopPropagation();
				dismiss();
				return;
		}
	}

	function bucketLabel(b: SessionListItem['bucket']): string {
		switch (b) {
			case 'blocked':
				return m.sessions_bucket_blocked();
			case 'review':
				return m.sessions_bucket_review();
			case 'done':
				return m.sessions_bucket_done();
			default:
				return m.sessions_bucket_working();
		}
	}
	const dirName = (d: string) => d.replace(/\/+$/, '').split('/').pop() || d;
</script>

<div
	class="mention-wrap"
	role="presentation"
	use:clickOutside={dismiss}
	onkeydowncapture={onKeydownCapture}
	oninput={refresh}
	onclick={refresh}
	onfocusin={refresh}
	onkeyup={(e) => {
		if (!open && (e.key.startsWith('Arrow') || e.key === 'Home' || e.key === 'End')) refresh();
	}}
	onfocusout={(e) => {
		// Tapping a row moves focus into the panel: keep it open for that.
		const to = e.relatedTarget as Node | null;
		if (to && e.currentTarget.contains(to)) return;
		close();
	}}
>
	{@render children()}
	{#if open}
		<div class="mention-panel" class:up role="listbox" aria-label={m.mention_list_aria()} bind:this={listEl}>
			{#each matches as s, i (s.id)}
				<button
					type="button"
					class="row"
					class:active={i === index}
					role="option"
					aria-selected={i === index}
					onpointerdown={(e) => e.preventDefault()}
					onclick={() => pick(s)}
					onpointerenter={() => (index = i)}
				>
					<span class="name">{s.name?.trim() || s.id}</span>
					<span class="meta">
						{#if s.name?.trim()}<span class="id">{s.id.slice(0, 8)}</span> · {/if}{#if s.machine_name}{s.machine_name} · {/if}{dirName(
							s.working_dir
						)} · {bucketLabel(s.bucket)}
					</span>
				</button>
			{:else}
				<div class="empty"><Text tone="muted" size="sm">{m.mention_empty()}</Text></div>
			{/each}
		</div>
	{/if}
</div>

<style>
	.mention-wrap {
		position: relative;
		flex: 1;
		min-width: 0;
	}
	.mention-panel {
		position: absolute;
		left: 0;
		right: 0;
		top: calc(100% + var(--sp-1));
		z-index: 40;
		display: flex;
		flex-direction: column;
		max-height: min(16rem, 40vh);
		overflow-y: auto;
		padding: var(--sp-1);
		border: 1px solid var(--border-strong);
		border-radius: var(--r-md);
		background: var(--bg-elevated);
		box-shadow: var(--shadow-lg);
	}
	/* Dropup: anchored to the field's top edge, growing upward. */
	.mention-panel.up {
		top: auto;
		bottom: calc(100% + var(--sp-1));
	}
	.row {
		display: flex;
		flex-direction: column;
		align-items: flex-start;
		gap: 2px;
		width: 100%;
		padding: var(--sp-1) var(--sp-2);
		border: 0;
		border-radius: var(--r-sm);
		background: transparent;
		color: var(--fg);
		font: inherit;
		text-align: left;
		cursor: pointer;
	}
	.row.active {
		background: color-mix(in srgb, var(--accent) 18%, transparent);
	}
	.name {
		overflow: hidden;
		white-space: nowrap;
		text-overflow: ellipsis;
		max-width: 100%;
		font-weight: var(--fw-semibold);
	}
	.meta {
		overflow: hidden;
		white-space: nowrap;
		text-overflow: ellipsis;
		max-width: 100%;
		color: var(--fg-muted);
		font-size: 0.8em;
	}
	.id {
		font-family: var(--font-mono);
	}
	.empty {
		padding: var(--sp-1) var(--sp-2);
	}
</style>
