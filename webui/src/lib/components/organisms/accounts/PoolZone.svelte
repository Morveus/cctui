<script lang="ts">
	import type { Snippet } from 'svelte';
	import type { AccountPoolView } from '@bindings/AccountPoolView';
	import type { PoolUsageView } from '@bindings/PoolUsageView';
	import type { OAuthAccount } from '$lib/queries';
	import { Fieldset, IconButton, Text } from '@dorsk/tsumikit';
	import { m } from '$lib/paraglide/messages';
	import { ACCOUNT_DRAG_MIME, acceptsDrop } from './pools.logic';
	import { accountDrag } from './drag.svelte';
	import PoolUsageGauges from './PoolUsageGauges.svelte';

	let {
		pool,
		accounts,
		usage = null,
		ownerName = null,
		onedit,
		ondrop,
		children
	}: {
		pool: AccountPoolView;
		/** Every account on the page, to judge a dropped id. */
		accounts: OAuthAccount[];
		/** The pool's aggregate usage, fetched once by the board; null while
		 *  loading or when the server has nothing for this pool. */
		usage?: PoolUsageView | null;
		/** Shown to admins, who see everyone's pools. */
		ownerName?: string | null;
		onedit?: () => void;
		ondrop?: (accountId: string) => void;
		children?: Snippet;
	} = $props();

	const meta = $derived(
		pool.failover
			? m.pools_legend_failover({ n: pool.members.length })
			: m.pools_legend({ n: pool.members.length })
	);
	const dragged = $derived(accounts.find((a) => a.id === accountDrag.accountId)?.name ?? '');

	// A refused zone never `preventDefault`s dragover, so the kit Fieldset stays
	// unlit and cannot tell us the pointer is here — the zone counts dragenter /
	// dragleave itself (nested children fire both, hence the depth).
	let depth = $state(0);
	const dragging = $derived(accountDrag.accountId !== '');
	const hovering = $derived(depth > 0 || accountDrag.overId === pool.id);
	const refused = $derived(
		dragging && hovering && !acceptsDrop(pool, accountDrag.accountId, accounts)
	);
	const isMember = $derived(pool.members.some((mem) => mem.account_id === accountDrag.accountId));
	const refusal = $derived(
		isMember ? m.pools_drop_refused_member({ name: dragged }) : m.pools_drop_refused({ name: dragged })
	);
	$effect(() => {
		if (!dragging) depth = 0;
	});
</script>

<div
	class="zone"
	class:over={accountDrag.overId === pool.id}
	class:refused
	data-pool-id={pool.id}
	data-journey="pool"
	data-journey-key={pool.name}
	title={refused ? refusal : undefined}
	role="none"
	ondragenter={() => depth++}
	ondragleave={() => (depth = Math.max(0, depth - 1))}
	ondrop={() => (depth = 0)}
>
<Fieldset
	tone="accent"
	dashed
	padding="sm"
	droppable
	mime={ACCOUNT_DRAG_MIME}
	accepts={(id) => acceptsDrop(pool, id || accountDrag.accountId, accounts)}
	ondrop={(id) => ondrop?.(id || accountDrag.accountId)}
	dropHint={m.pools_drop_hint({ name: dragged })}
	class="pool"
>
	{#snippet legend()}
		<Text as="span" size="sm" weight="semibold" tone="accent">{pool.name}</Text>
		<Text as="span" size="xs" tone="faint">{ownerName ? `${ownerName} · ${meta}` : meta}</Text>
		{#if onedit}
			<IconButton icon="edit" label={m.pools_edit()} inline size={13} onclick={onedit} />
		{/if}
	{/snippet}
	<div class="content" class:with-usage={usage !== null}>
		{#if usage}
			<div class="gauges">
				<PoolUsageGauges {usage} />
			</div>
		{/if}
		<div class="members">
			{@render children?.()}
			{#if pool.members.length === 0}
				<Text as="p" tone="faint" size="sm">{m.pools_members_empty()}</Text>
			{/if}
		</div>
	</div>
	{#if refused}
		<p class="refusal" aria-live="polite">{refusal}</p>
	{/if}
</Fieldset>
</div>

<style>
	.zone {
		container-type: inline-size;
		border-radius: var(--r-lg);
	}
	/* Touch drag hover: the kit Fieldset only reacts to HTML5 dragover. */
	.zone.over {
		outline: 2px solid var(--accent);
		outline-offset: 2px;
	}
	.zone.refused {
		outline: 2px dashed var(--danger);
		outline-offset: 2px;
	}
	/* Overlaid, not in flow: a banner that grew the zone would shove the other
	   pools out from under the pointer mid-drag. */
	.refusal {
		position: absolute;
		left: 50%;
		bottom: var(--sp-2);
		transform: translateX(-50%);
		margin: 0;
		padding: var(--sp-2) var(--sp-3);
		border: 1px dashed var(--danger);
		border-radius: var(--r-md);
		background: color-mix(in srgb, var(--danger) 12%, var(--bg-elevated));
		color: var(--danger);
		font-size: var(--fs-sm);
		text-align: center;
		pointer-events: none;
	}
	.members {
		min-width: 0;
		display: flex;
		flex-direction: column;
		gap: var(--sp-3);
	}
	/* Stack on narrow panels; use a separate summary column when both fit. */
	.gauges {
		min-width: 0;
		margin-bottom: var(--sp-3);
		padding-bottom: var(--sp-2);
		border-bottom: 1px dashed var(--border);
	}
	@container (min-width: 56rem) {
		.content.with-usage {
			display: grid;
			grid-template-columns: minmax(18rem, 1fr) minmax(0, 2fr);
			gap: var(--sp-4);
			align-items: start;
		}
		.with-usage .gauges {
			margin-bottom: 0;
			padding-bottom: 0;
			padding-inline-end: var(--sp-4);
			border-bottom: 0;
			border-inline-end: 1px dashed var(--border);
		}
	}
</style>
