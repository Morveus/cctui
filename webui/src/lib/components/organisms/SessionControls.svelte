<script lang="ts">
	import type { Label } from '@bindings/Label';
	import type { Section } from '../../../routes/sessions/sessions.logic';
	import { Button, Field, FilterSearchBar, Heading, Icon, Popover, type Schema } from '@dorsk/tsumikit';
	import { m } from '$lib/paraglide/messages';
	import { sessionSearchPlaceholder } from '$lib/searchSchema';
	import SectionFilter from '../molecules/SectionFilter.svelte';
	import LabelFilter from '../molecules/LabelFilter.svelte';
	import ViewPicker from '../molecules/ViewPicker.svelte';
	import DimensionPicker from '../molecules/DimensionPicker.svelte';
	import MacrosMenu from './MacrosMenu.svelte';
	import { settings } from '$lib/settings.svelte';
	import type { Dimension } from '../../../routes/sessions/sessions.logic';

	// The sessions list toolbar: title + search + section/label filters +
	// view picker + multi-select toggle + New. A uniform, self-contained block —
	// each control is its own molecule so this layer only owns composition and the
	// responsive bar layout. Two-way state stays owned by the page (persisted to
	// drafts) and flows in via bindable props.
	let {
		rawQuery = $bindable(),
		searchSchema,
		sections = $bindable(),
		labels,
		labelFilter = $bindable(),
		cardView = $bindable(),
		colorBy,
		groupBy,
		onColorBy,
		onGroupBy,
		selecting,
		searching,
		onStartSelect,
		onCancelSelect,
		onNew,
		onUpdateLabel,
		onDeleteLabel
	}: {
		rawQuery: string;
		searchSchema: Schema;
		sections: Set<Section>;
		labels: Label[];
		labelFilter: Set<string>;
		cardView: boolean;
		colorBy: Dimension;
		groupBy: Dimension;
		onColorBy: (v: Dimension) => void;
		onGroupBy: (v: Dimension) => void;
		selecting: boolean;
		searching: boolean;
		onStartSelect: () => void;
		onCancelSelect: () => void;
		// Absent when the docked spawn panel replaces the "+ New" button.
		onNew?: () => void;
		onUpdateLabel?: (labelId: string, patch: { name?: string; color?: string }) => Promise<Label>;
		onDeleteLabel?: (labelId: string) => void | Promise<void>;
	} = $props();

	const searchId = $props.id();

	// Button centres its content; a full-width menu row reads left-aligned like
	// the picker rows beside it. No `align` prop on Button yet (OptionButton has one).
	const MENU_ROW = 'justify-content:flex-start';

	// Overflow menu: the toolbar grew too many buttons and squeezed the
	// search bar. A ⋯ Popover collapses the secondary controls — the native
	// popover gives light dismiss, Escape and focus return. On desktop it holds
	// the two DimensionPickers (color-by · group-by) so the search bar reclaims
	// width; below the fold the label/view/select controls move in too, leaving
	// only the section filter inline.
	//
	// The fold is measured, not queried: the panel renders in the top layer, so a
	// `@container` rule on the bar cannot reliably style its contents. The bar's
	// own width still drives it (not the viewport, mirroring DrawerHeader) at the
	// same threshold as the row reorg below.
	const FOLD_W = 640;
	let barEl = $state<HTMLElement | null>(null);
	let narrow = $state(false);

	$effect(() => {
		if (!barEl || typeof ResizeObserver === 'undefined') return;
		const el = barEl;
		const ro = new ResizeObserver(() => (narrow = el.clientWidth <= FOLD_W));
		ro.observe(el);
		return () => ro.disconnect();
	});
</script>

<!-- Controls that stay inline on desktop but fold into the ⋯ menu on narrow
     widths: label filter, view picker, multi-select toggle. Rendered
     via a snippet so the inline copy and the menu copy share one source. -->
{#snippet listChecks()}
	<Icon label={m.sessions_select_multiple()} size={18}>
		<path d="m3 17 2 2 4-4" />
		<path d="m3 7 2 2 4-4" />
		<path d="M13 6h8" />
		<path d="M13 12h8" />
		<path d="M13 18h8" />
	</Icon>
{/snippet}
{#snippet foldControls(menu: boolean)}
	<LabelFilter {menu} {labels} bind:selected={labelFilter} onUpdate={onUpdateLabel} onDelete={onDeleteLabel} />
	<ViewPicker {menu} bind:cardView />
	<!-- Stays mounted (disabled) while searching: unmounting it re-wraps the
	     flex bar mid-type and makes the search field jump. -->
	{#if selecting}
		<!-- Cancel selection. -->
		{#if menu}
			<Button variant="ghost" size="sm" block style={MENU_ROW} onclick={onCancelSelect}>
				<Icon name="x" size={18} /><span>{m.sessions_cancel_selection()}</span>
			</Button>
		{:else}
			<Button class="ctl" square title={m.sessions_cancel_selection()} aria-label={m.sessions_cancel_selection()} onclick={onCancelSelect}>
				<Icon name="x" size={18} />
			</Button>
		{/if}
	{:else if menu}
		<Button variant="ghost" size="sm" block style={MENU_ROW} disabled={searching} onclick={onStartSelect}>
			{@render listChecks()}<span>{m.sessions_select_multiple()}</span>
		</Button>
	{:else}
		<!-- "Select multiple" wants a checklist/multi-select glyph the registry
		     doesn't ship; feed Icon a raw list-checks svg via its children. -->
		<Button class="ctl" square disabled={searching} title={m.sessions_select_multiple()} aria-label={m.sessions_select_multiple()} onclick={onStartSelect}>
			{@render listChecks()}
		</Button>
	{/if}
{/snippet}

<div class="bar row" bind:this={barEl}>
	<span class="title-wrap">
		<Heading level={1} size="xl">{m.sessions_title()}</Heading>
	</span>
	<!-- FilterSearchBar forwards no id/aria-label, so the name reaches its input
	     through the Field context; the label itself is screen-reader only. -->
	<div class="search-box" data-journey="search">
		<label for={searchId} class="sr-only">{m.a11y_sessions_search()}</label>
		<Field for={searchId}>
			<FilterSearchBar
				schema={searchSchema}
				bind:value={rawQuery}
				placeholder={sessionSearchPlaceholder()}
			/>
		</Field>
	</div>
	<span class="ctl-item"><SectionFilter bind:sections /></span>
	<!-- Above the fold the foldable controls render inline as bar-level flex items
	     (display:contents); below it they move into the ⋯ menu instead. -->
	{#if !narrow}
		<div class="inline-fold">{@render foldControls(false)}</div>
	{/if}
	<span class="ctl-item">
		<Popover
			label={m.drawer_more_actions()}
			title={m.drawer_more_actions()}
			data-journey="options"
			placement="bottom-end"
			role="menu"
			haspopup="menu"
			variant="default"
			box="lg"
			panelStyle="width:15rem;box-shadow:var(--shadow-lg)"
		>
			{#snippet trigger()}
				<Icon name="more" size={18} />
			{/snippet}
			<!-- The two DimensionPickers live here at all widths; below the fold the
			     foldable controls join them. -->
			<div class="menu" data-journey="display-options">
				{#if narrow}{@render foldControls(true)}{/if}
				<DimensionPicker menu kind="group" value={groupBy} onchange={onGroupBy} />
				<DimensionPicker menu kind="color" value={colorBy} onchange={onColorBy} />
			</div>
		</Popover>
	</span>
	{#if settings.macrosEnabled}
		<span class="new-wrap">
			<MacrosMenu />
		</span>
	{/if}
	{#if onNew}
		<span class="new-wrap">
			<Button data-journey="new" variant="primary" shrink={false} title={m.sessions_new_session()} aria-label={m.sessions_new_session()} onclick={onNew}>+<span class="new-label"> {m.sessions_new()}</span></Button>
		</span>
	{/if}
	<!-- Mobile-only flex row-break: basis:100% forces row 2 (search +
	     tools) onto a fresh line below title+New. Hidden on desktop where everything
	     sits on one row. -->
	<span class="row-break break-tools" aria-hidden="true"></span>
</div>

<style>
	.bar {
		/* Sticky under the fixed app header so the controls stay reachable on long
		   lists without scrolling back up. */
		position: sticky;
		top: calc(var(--header-h) + var(--safe-top));
		z-index: 6;
		margin-bottom: var(--sp-4);
		/* Pad the bottom only: top padding would drop the title below the header
		   baseline every other page aligns to. */
		padding: 0 0 var(--sp-2);
		gap: var(--sp-2);
		align-items: center;
		/* Wrap so controls reflow onto a second line instead of overflowing when
		   the UI scale grows the title/buttons. */
		flex-wrap: wrap;
		background: var(--bg);
		/* Drive the row reorg from the bar's own width, not the viewport, mirroring
		   DrawerHeader. */
		container: sess-bar / inline-size;
	}
	/* Inline copy of the foldable controls flows as bar-level flex items. */
	.inline-fold {
		display: contents;
	}
	/* Labeled rows stacked like a real menu; the panel supplies the surface. */
	.menu {
		display: flex;
		flex-direction: column;
		align-items: stretch;
		gap: 2px;
	}
	/* Search fills the gap between the title and the right-hand controls. Our
	   own wrapper is the flex item and is sized directly, so the FilterSearchBar
	   root fills it (block, width:100%) and its below-bar chips stack onto their
	   own row within the wrapper — opening/typing never moves the input or the
	   surrounding controls. Sized here, not via any library
	   internal class, so a tsumikit internal-class rename can't break it. */
	.search-box {
		flex: 1 1 0;
		min-width: 0;
	}
	.title-wrap {
		display: flex;
		align-self: center;
		min-width: 0;
	}
	/* Each bar child is a local element so the narrow-width `order` reshuffle
	   below never has to reach into a child component's root. */
	.ctl-item,
	.new-wrap {
		display: flex;
		flex: none;
	}
	.new-label {
		display: inline;
	}
	/* Row-break helpers: zero-height flex items with a full-width basis. Off by
	   default (single-row desktop bar); switched on in the mobile query below. */
	.row-break {
		display: none;
	}
	/* Narrow bar: two rows — row 1 title + full "+ New", row 2
	   the search bar (which shrinks to fill) followed by the tool controls on the
	   right. `order` sequences the items; the row-break forces the single wrap.
	   Driven by the SAME container query as the fold above (not a viewport media
	   query): the bar is often narrower than the viewport, so a viewport breakpoint
	   left a dead band where controls folded but the reorg didn't fire, orphaning
	   the tools onto a lonely row while search stayed cramped on row 1. */
	@container sess-bar (max-width: 640px) {
		/* Default everyone to row 2… */
		.search-box,
		.ctl-item {
			order: 2;
		}
		/* Row 1: title (grows to push New flush right) then the New button. */
		.title-wrap {
			order: 0;
			flex: 1 1 auto;
		}
		.new-wrap {
			order: 1;
		}
		/* Break after row 1: forces search + tools onto row 2. height:0 + negative
		   row-gap cancel so the phantom line adds no vertical space. */
		.break-tools {
			display: block;
			order: 1;
			flex: 0 0 100%;
			height: 0;
			margin-top: calc(-1 * var(--sp-2));
		}
		/* Row 2: search takes only the leftover space (flex:1 1 0 from the base
		   rule, so it never demands its intrinsic input width) and shrinks freely;
		   the tools follow on the right, all on one row. It picks up order:2 from
		   the `.bar > *` reset above as a real (non-contents) flex item. */
	}
</style>
