<script lang="ts">
	import type { Label } from '@bindings/Label';
	import { Button, Icon } from '@dorsk/tsumikit';
	import { m } from '$lib/paraglide/messages';
	import { clickOutside } from '$lib/clickOutside';
	import LabelMenu from './LabelMenu.svelte';

	// One square toolbar button that opens a popover of label toggles; a session
	// shows when it carries ANY selected label (OR semantics). The menu
	// body is the shared LabelMenu molecule; this wrapper owns the
	// trigger, the count badge and the open/close (clickOutside). `selected` is
	// bindable so the parent owns persistence. Renders nothing until at least one
	// label exists, so the caller doesn't have to guard.
	let {
		labels,
		selected = $bindable(),
		onUpdate,
		onDelete,
		menu = false
	}: {
		labels: Label[];
		selected: Set<string>;
		/** Render as a full-width labeled row for the overflow ⋯ menu. */
		menu?: boolean;
		// Editing the labels themselves (rename/recolor/delete) from the filter
		// menu — the same edit affordance the per-session picker has.
		onUpdate?: (labelId: string, patch: { name?: string; color?: string }) => Promise<Label>;
		onDelete?: (labelId: string) => void | Promise<void>;
	} = $props();

	let open = $state(false);

	function toggle(l: Label) {
		const next = new Set(selected);
		if (next.has(l.id)) next.delete(l.id);
		else next.add(l.id);
		selected = next;
	}
</script>

{#if labels.length > 0}
	<div class="label-filter" class:menu-row={menu} use:clickOutside={() => (open = false)}>
		{#if menu}
			<Button
				variant="ghost"
				size="sm"
				block
				style="justify-content:flex-start"
				tone={selected.size > 0 ? 'accent' : 'none'}
				title={selected.size > 0 ? m.sessions_filtering_by_labels({ count: selected.size }) : m.sessions_filter_by_label()}
				aria-haspopup="true"
				aria-expanded={open}
				aria-pressed={selected.size > 0}
				onclick={() => (open = !open)}
			>
				<Icon name="tag" size={18} />
				<span>{m.sessions_filter_by_label()}</span>
				{#if selected.size > 0}<span class="menu-count" aria-hidden="true">{selected.size}</span>{/if}
			</Button>
		{:else}
			<Button
				square
				aria-label={m.sessions_filter_by_label()}
				title={selected.size > 0 ? m.sessions_filtering_by_labels({ count: selected.size }) : m.sessions_filter_by_label()}
				aria-haspopup="true"
				aria-expanded={open}
				aria-pressed={selected.size > 0}
				onclick={() => (open = !open)}
			>
				<Icon name="tag" size={18} />
			</Button>
			{#if selected.size > 0}<span class="count-badge" aria-hidden="true">{selected.size}</span>{/if}
		{/if}
		{#if open}
			<!-- svelte-ignore a11y_no_static_element_interactions -->
			<div
				class="menu"
				role="menu"
				aria-label={m.sessions_labels_menu()}
				tabindex="-1"
				onkeydown={(e) => {
					if (e.key === 'Escape') open = false;
				}}
			>
				<LabelMenu
					{labels}
					selectedIds={selected}
					cap={5}
					autofocus
					onToggle={toggle}
					onClear={() => (selected = new Set())}
					{onUpdate}
					{onDelete}
				/>
			</div>
		{/if}
	</div>
{/if}

<style>
	.label-filter {
		position: relative;
		display: inline-flex;
		align-items: center;
		flex: none;
	}
	/* Overflow-menu row: full-width, left-aligned icon + label, matching the
	   drawer's ⋯ flyout rows. */
	.label-filter.menu-row {
		display: flex;
		width: 100%;
	}
	.menu-count {
		margin-left: auto;
		min-width: 1.25rem;
		height: 1.25rem;
		padding: 0 0.35rem;
		border-radius: 999px;
		background: var(--accent);
		color: var(--bg);
		font-size: 0.7rem;
		font-weight: var(--fw-semibold);
		line-height: 1.25rem;
		text-align: center;
	}
	.count-badge {
		position: absolute;
		top: -0.35rem;
		right: -0.35rem;
		min-width: 1rem;
		height: 1rem;
		padding: 0 0.25rem;
		border-radius: 999px;
		background: var(--accent);
		color: var(--bg);
		font-size: 0.62rem;
		font-weight: var(--fw-semibold);
		line-height: 1rem;
		text-align: center;
		pointer-events: none;
	}
	.menu {
		position: absolute;
		top: calc(100% + var(--sp-1));
		right: 0;
		z-index: 40;
		display: flex;
		flex-direction: column;
		padding: var(--sp-1);
		border: 1px solid var(--border-strong);
		border-radius: var(--r-md);
		background: var(--bg-elevated);
		box-shadow: var(--shadow-lg);
	}
</style>
