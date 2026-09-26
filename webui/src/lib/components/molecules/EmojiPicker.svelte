<script lang="ts">
	// Emoji chooser for the account glyph: a Popover holding a search box and a
	// grid of the embedded catalogue, grouped by tabs. Desktop browsers have no
	// emoji keyboard, so this is the click-only path; the text field beside it
	// stays for anyone who can type or paste one.
	import { Input, Popover } from '@dorsk/tsumikit';
	import { m } from '$lib/paraglide/messages';
	import { EMOJI_GROUPS, searchEmoji } from './emojiCatalog';

	let {
		value = '',
		onselect
	}: {
		/** The current glyph, highlighted in the grid. */
		value?: string;
		onselect: (emoji: string) => void;
	} = $props();

	let query = $state('');
	let groupId = $state(EMOJI_GROUPS[0].id);
	let root: HTMLDivElement | undefined = $state();
	// The kit Popover owns the native `popover="auto"` panel; closing it after a
	// pick means hiding that ancestor.
	const close = () => (root?.closest('[popover]') as HTMLElement | null)?.hidePopover?.();

	const hits = $derived(searchEmoji(query));
	const searching = $derived(query.trim().length > 0);
	const shown = $derived(
		searching ? hits : (EMOJI_GROUPS.find((g) => g.id === groupId) ?? EMOJI_GROUPS[0]).entries
	);

	function pick(emoji: string) {
		onselect(emoji);
		query = '';
		close();
	}
</script>

<Popover
	label={m.emoji_picker_open()}
	placement="bottom-start"
	control
	onopen={() => (query = '')}
	panelClass="emoji-panel"
	role="dialog"
>
	{#snippet trigger()}
		<span class="trigger" aria-hidden="true">{value.trim() || '🙂'}</span>
	{/snippet}
	<div class="picker" bind:this={root}>
		<Input
			bind:value={query}
			placeholder={m.emoji_picker_search()}
			aria-label={m.emoji_picker_search()}
			size="sm"
			clearable
			onclear={() => (query = '')}
			style="width:100%"
		/>
		{#if !searching}
			<div class="tabs" role="tablist">
				{#each EMOJI_GROUPS as g (g.id)}
					<button
						type="button"
						role="tab"
						class="tab"
						class:on={g.id === groupId}
						aria-selected={g.id === groupId}
						aria-label={g.id}
						onclick={() => (groupId = g.id)}
					>
						{g.icon}
					</button>
				{/each}
			</div>
		{/if}
		<div class="grid" role="listbox" aria-label={m.emoji_picker_open()}>
			{#each shown as x (x.emoji)}
				<button
					type="button"
					role="option"
					class="cell"
					class:on={x.emoji === value.trim()}
					aria-selected={x.emoji === value.trim()}
					title={x.keys}
					onclick={() => pick(x.emoji)}
				>
					{x.emoji}
				</button>
			{:else}
				<p class="empty">{m.emoji_picker_empty()}</p>
			{/each}
		</div>
	</div>
</Popover>

<style>
	.trigger {
		font-size: 1.1rem;
		line-height: 1;
	}
	.picker {
		display: flex;
		flex-direction: column;
		gap: var(--sp-2);
		width: min(20rem, calc(100vw - 2rem));
	}
	.tabs {
		display: flex;
		gap: 2px;
	}
	.tab,
	.cell {
		border: 1px solid transparent;
		background: none;
		border-radius: var(--r-sm);
		cursor: pointer;
		font-size: 1.25rem;
		line-height: 1;
		padding: 0.3rem;
		color: inherit;
	}
	.tab.on,
	.cell.on {
		border-color: var(--accent);
	}
	.tab:hover,
	.cell:hover,
	.tab:focus-visible,
	.cell:focus-visible {
		background: var(--bg-elevated-2);
		outline: none;
	}
	.grid {
		display: grid;
		grid-template-columns: repeat(8, 1fr);
		gap: 2px;
		max-height: 14rem;
		overflow-y: auto;
	}
	.empty {
		grid-column: 1 / -1;
		margin: var(--sp-2) 0;
		font-size: var(--fs-xs);
		color: var(--c-muted);
		text-align: center;
	}
</style>
