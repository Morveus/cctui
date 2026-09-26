<script lang="ts">
	import ImageCompressionStatus from '$lib/components/molecules/ImageCompressionStatus.svelte';
	import type { Label } from '@bindings/Label';
	import { AutoGrid, Badge, Button, FileButton, Icon } from '@dorsk/tsumikit';
	import { clickOutside } from '$lib/clickOutside';
	import { labelTint, hueToColor } from '$lib/labels';
	import AttachmentList from '$lib/components/molecules/AttachmentList.svelte';
	import LabelMenu from '$lib/components/molecules/LabelMenu.svelte';
	import EnvSecretsField from './EnvSecretsField.svelte';
	import type { EnvRow } from './types';
	import { m } from '$lib/paraglide/messages';

	let {
		labelIds = $bindable(),
		envRows = $bindable(),
		pending = [],
		files,
		allLabels,
		envInvalid,
		attachments,
		labelActions,
		onfiles,
		onremovefile
	}: {
		labelIds: string[];
		envRows: EnvRow[];
		pending?: { file: File }[];
		files: File[];
		allLabels: Label[];
		envInvalid: boolean;
		attachments: boolean;
		labelActions: {
			createLabel: (name: string, color: string) => Promise<Label>;
			updateLabel: (id: string, patch: { name?: string; color?: string }) => Promise<Label>;
			deleteLabel: (id: string) => Promise<void>;
		};
		onfiles: (files: File[]) => void;
		onremovefile: (name: string) => void;
	} = $props();

	const selectedLabels = $derived(allLabels.filter((l) => labelIds.includes(l.id)));
	const attachedLabelIds = $derived(new Set(labelIds));
	function toggleLabel(l: Label) {
		labelIds = labelIds.includes(l.id) ? labelIds.filter((x) => x !== l.id) : [...labelIds, l.id];
	}
	async function createAndAttach(name: string) {
		if (!name.trim()) return;
		const label = await labelActions.createLabel(name, hueToColor(null));
		if (!labelIds.includes(label.id)) labelIds = [...labelIds, label.id];
	}

	// The panel uses the native popover API so it renders in the top layer,
	// above the Modal's <dialog> and outside its scrolling body; placed from the
	// trigger rect, flipped above when it would overflow the viewport.
	//
	// The placement is redone while the menu is open on every viewport change:
	// on phones the soft keyboard shrinks the viewport (interactive-widget=
	// resizes-content in app.html) AFTER the search box takes focus, and a
	// fixed panel placed once would keep its search box under the keyboard.
	// The panel is then pushed up until its bottom edge is back on screen, and
	// capped to the visible height so the search box always stays reachable.
	let menuOpen = $state(false);
	let triggerEl = $state<HTMLElement | null>(null);
	let menuEl = $state<HTMLElement | null>(null);
	let menuPos = $state({ top: 0, left: 0 });
	let menuMaxHeight = $state<number | null>(null);
	$effect(() => {
		if (!menuOpen) return;
		const vv = window.visualViewport;
		window.addEventListener('resize', placeMenu);
		vv?.addEventListener('resize', placeMenu);
		vv?.addEventListener('scroll', placeMenu);
		return () => {
			window.removeEventListener('resize', placeMenu);
			vv?.removeEventListener('resize', placeMenu);
			vv?.removeEventListener('scroll', placeMenu);
		};
	});
	function openMenu() {
		if (!triggerEl) return;
		const r = triggerEl.getBoundingClientRect();
		menuPos = { top: r.bottom + 4, left: r.left };
		menuOpen = true;
		menuEl?.showPopover();
		requestAnimationFrame(placeMenu);
	}
	function placeMenu() {
		if (!triggerEl || !menuEl) return;
		const gap = 4;
		const vv = window.visualViewport;
		const vTop = vv?.offsetTop ?? 0;
		const vLeft = vv?.offsetLeft ?? 0;
		const vHeight = vv?.height ?? window.innerHeight;
		const vWidth = vv?.width ?? window.innerWidth;
		const t = triggerEl.getBoundingClientRect();
		menuMaxHeight = Math.max(0, vHeight - 2 * gap);
		const p = menuEl.getBoundingClientRect();
		const height = Math.min(p.height, menuMaxHeight);
		const spaceBelow = vTop + vHeight - t.bottom;
		const flipUp = spaceBelow < height + gap && t.top - vTop > spaceBelow;
		let top = flipUp ? t.top - height - gap : t.bottom + gap;
		// Keep the whole panel inside the visible area: pushed up when its
		// bottom would fall under the keyboard, never above the top edge.
		top = Math.min(top, vTop + vHeight - height - gap);
		top = Math.max(vTop + gap, top);
		const left = Math.max(vLeft + gap, Math.min(t.left, vLeft + vWidth - p.width - gap));
		menuPos = { top, left };
	}
	function closeMenu() {
		if (!menuOpen) return;
		menuOpen = false;
		menuEl?.hidePopover();
	}
	const toggleMenu = () => (menuOpen ? closeMenu() : openMenu());
	const addEnvRow = () => (envRows = [...envRows, { key: '', value: '' }]);
</script>

<div class="addons">
	<span class="addon-title">{m.spawn_optional_settings()}</span>
	<!-- Button labels never wrap, so the column floor must fit the longest localized
	     label plus icon ("Fichiers" / "Env vars"); short labels let three fit on a phone. -->
	<AutoGrid min="8rem" gap="var(--sp-2)" maxCols={3} align="stretch">
		<div class="label-add" bind:this={triggerEl} use:clickOutside={closeMenu}>
			<Button block aria-haspopup="true" aria-expanded={menuOpen} onclick={toggleMenu}>
				<Icon name="tag" />{m.spawn_add_label()}
			</Button>
			<div
				bind:this={menuEl}
				class="label-menu"
				popover="manual"
				role="menu"
				aria-label={m.spawn_labels_aria()}
				tabindex="-1"
				style:top="{menuPos.top}px"
				style:left="{menuPos.left}px"
				style:max-height={menuMaxHeight === null ? null : `${menuMaxHeight}px`}
				onkeydown={(e) => {
					if (e.key === 'Escape') closeMenu();
				}}
			>
				{#if menuOpen}
					<LabelMenu
						labels={allLabels}
						selectedIds={attachedLabelIds}
						cap={5}
						autofocus
						onToggle={toggleLabel}
						onCreate={createAndAttach}
						onUpdate={(labelId, patch) => labelActions.updateLabel(labelId, patch)}
						onDelete={(labelId) => labelActions.deleteLabel(labelId)}
					/>
				{/if}
			</div>
		</div>
		{#if attachments}
			<FileButton label={m.spawn_add_files()} icon="file-text" multiple {onfiles} />
		{/if}
		<Button block onclick={addEnvRow}><Icon name="plus" />{m.spawn_add_env_vars()}</Button>
	</AutoGrid>

	{#if selectedLabels.length}
		<div class="addon-labels">
			{#each selectedLabels as l (l.id)}
				<Badge
					style="{labelTint(l)};border-radius:var(--r-sm)"
					removable
					onremove={() => (labelIds = labelIds.filter((x) => x !== l.id))}
				>
					{l.name}
				</Badge>
			{/each}
		</div>
	{/if}
	{#if attachments}
		<ImageCompressionStatus {pending} />
		<AttachmentList {files} onremove={onremovefile} />
	{/if}
	<EnvSecretsField bind:envRows invalid={envInvalid} />
</div>

<style>
	.addons {
		display: flex;
		flex-direction: column;
		gap: var(--sp-2);
	}
	.addon-title {
		font-size: var(--fs-sm);
		font-weight: var(--fw-medium);
		color: var(--text-muted);
	}
	.addon-labels {
		display: flex;
		flex-wrap: wrap;
		gap: var(--sp-1);
	}
	.label-add {
		display: flex;
		align-items: stretch;
	}
	.label-menu {
		position: fixed;
		inset: auto;
		margin: 0;
		padding: var(--sp-1);
		display: flex;
		flex-direction: column;
		border: 1px solid var(--border-strong);
		border-radius: var(--r-md);
		background: var(--bg-elevated);
		box-shadow: var(--shadow-lg);
		overflow-y: auto;
	}
	.label-menu:not(:popover-open) {
		display: none;
	}
</style>
