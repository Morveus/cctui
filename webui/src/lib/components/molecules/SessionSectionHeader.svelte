<script lang="ts">
	import type { Snippet } from 'svelte';
	import { Icon, IconButton, Menu, SectionHeader, Text, type IconName } from '@dorsk/tsumikit';
	import type { MenuItem } from '@dorsk/tsumikit';
	import { m } from '$lib/paraglide/messages';

	type SortField = 'activity' | 'created' | 'name';
	type SortDir = 'asc' | 'desc';

	let {
		label,
		title,
		count,
		hue,
		lead,
		sort,
		sortDir,
		onsort,
		hidden,
		ontogglehidden,
		onarchive,
		archiving = false
	}: {
		label: string;
		// Rendered title; empty when `lead` already names the section.
		title?: string;
		count: number;
		hue?: number;
		lead?: Snippet;
		sort: SortField;
		sortDir: SortDir;
		onsort: (field: SortField) => void;
		hidden: boolean;
		ontogglehidden: () => void;
		onarchive?: () => void;
		archiving?: boolean;
	} = $props();

	const FIELDS: SortField[] = ['activity', 'created', 'name'];
	const fieldLabel = (f: SortField): string =>
		f === 'created'
			? m.settings_sort_created()
			: f === 'name'
				? m.settings_sort_name()
				: m.settings_sort_activity();
	const dirIcon = $derived<IconName>(sortDir === 'asc' ? 'arrow-up' : 'arrow-down');
	const dirLabel = $derived(sortDir === 'asc' ? m.sessions_sort_asc() : m.sessions_sort_desc());
	const sortItems = $derived<MenuItem[]>(
		FIELDS.map((f) => ({
			label: fieldLabel(f),
			pressed: f === sort,
			icon: f === sort ? dirIcon : undefined,
			onselect: () => onsort(f)
		}))
	);
	const eyeLabel = $derived(
		hidden ? m.sessions_section_show({ section: label }) : m.sessions_section_hide({ section: label })
	);
	const archiveLabel = $derived(m.sessions_archive_section({ section: label }));
	const heading = $derived(title ?? label);
	let width = $state(Infinity);
	// Mirrors the `ssh` container query: a bare number once the row is tight.
	const countLabel = $derived(width < 416 ? String(count) : m.sessions_group_count({ count }));
</script>

<div class="ssh" bind:clientWidth={width}>
	<SectionHeader
		variant="group"
		level={3}
		size="sm"
		title={heading}
		{hue}
		count={countLabel}
		{lead}
		actions={headerActions}
	/>
</div>

{#snippet headerActions()}
	<Menu label={m.sessions_sort_menu_label()} items={sortItems} bare placement="bottom-end">
		{#snippet trigger()}
			<Text
				data-journey="group-sort"
				size="xs"
				tone="faint"
				style="white-space:nowrap; display:inline-flex; align-items:center; gap: var(--sp-1)"
				><span class="sort-words">{m.sessions_sort_menu({ sort: fieldLabel(sort) })}</span><Icon
					name={dirIcon}
					label={dirLabel}
				/></Text
			>
		{/snippet}
	</Menu>
	<IconButton
		inline
		data-journey="group-hide"
		icon={hidden ? 'eye' : 'eye-off'}
		size={14}
		label={eyeLabel}
		title={eyeLabel}
		onclick={ontogglehidden}
	/>
	{#if onarchive}
		<IconButton
			inline
			data-journey="group-archive"
			icon="archive"
			size={14}
			label={archiveLabel}
			title={archiveLabel}
			disabled={archiving || count === 0}
			data-testid="archive-section"
			onclick={onarchive}
		/>
	{/if}
{/snippet}

<style>
	.ssh {
		container: ssh / inline-size;
	}
	@container ssh (max-width: 26rem) {
		.sort-words {
			display: none;
		}
	}
</style>
