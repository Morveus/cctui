<script lang="ts">
	import { untrack } from 'svelte';
	import { errMessage } from '$lib/api';
	import {
		useAccountActions,
		useAccountUsage,
		useSettingsCatalog,
		type AccountProvider,
		type OAuthAccount
	} from '$lib/queries';
	import { useQueryClient } from '@tanstack/svelte-query';
	import { toasts } from '$lib/toast.svelte';
	import { providerLabel } from '$lib/providers';
	import { m } from '$lib/paraglide/messages';
	import { Button, Drawer, IconButton, NavItem, Text, resizeHandle } from '@dorsk/tsumikit';
	import AdapterIcon from '$lib/components/atoms/AdapterIcon.svelte';
	import UsageNoticesEditor from '$lib/components/molecules/UsageNoticesEditor.svelte';
	import FireworksProviderEditor from '$lib/components/organisms/FireworksProviderEditor.svelte';
	import { editorWindowKeys } from '$lib/components/molecules/usage-windows';
	import { pagesFor, type PageId } from './pages.logic';
	import { knobGroups, knobKeyNames } from './knobs.logic';
	import { ProviderEdit } from './editor.svelte';
	import AliasesPage from './AliasesPage.svelte';
	import LimitsPage from './LimitsPage.svelte';
	import ModelsPage from './ModelsPage.svelte';
	import SettingsPage from './SettingsPage.svelte';
	import AdvancedPage from './AdvancedPage.svelte';

	let {
		account,
		provider,
		accounts = [],
		initialPage,
		onclose
	}: {
		account: OAuthAccount;
		provider: AccountProvider;
		/** Same-owner move targets are picked from here. */
		accounts?: OAuthAccount[];
		/** Deep-link target; ignored when this provider has no such page. */
		initialPage?: PageId;
		onclose: () => void;
	} = $props();

	const p = untrack(() => provider);
	const kind = p.provider;
	const edit = new ProviderEdit(p);
	const pages = pagesFor(kind);
	const landing = untrack(() => initialPage);
	if (landing && pages.includes(landing)) edit.page = landing;

	const actions = useAccountActions();
	const qc = useQueryClient();
	const catalog = useSettingsCatalog(() => p.family ?? 'anthropic');
	const usage = useAccountUsage(
		() => p.id,
		() => kind === 'anthropic' || kind === 'openai' || kind === 'fireworks'
	);
	const windows = $derived(usage.data?.windows ?? []);
	const softRows = $derived(
		editorWindowKeys(
			windows,
			p.soft_limits ?? null,
			edit.isFireworks ? 'fireworks' : (p.family ?? null)
		)
	);
	$effect(() => edit.seedWindows(softRows.map((r) => r.key)));

	const groups = $derived(knobGroups(catalog.data));
	const groupsOn = (id: PageId) => (edit.hasCatalog ? groups.filter((g) => g.page === id) : []);
	const claimed = $derived(new Set(groups.flatMap((g) => knobKeyNames(g.knobs)).concat(['env'])));
	const settableKeys = $derived(new Set((catalog.data?.keys ?? []).map((k) => k.name)));
	const catalogLoading = $derived(edit.hasCatalog && !catalog.data && !catalog.error);
	const catalogFailed = $derived(edit.hasCatalog && !!catalog.error);
	const changes = $derived(edit.changes(groupsOn(edit.page)));

	const LABELS: Record<PageId, () => string> = {
		aliases: m.provider_page_aliases,
		limits: m.provider_page_limits,
		ui: m.provider_page_ui,
		privacy: m.provider_page_privacy,
		tools: m.provider_page_tools,
		speed: m.provider_page_speed,
		reasoning: m.provider_page_reasoning,
		gateway: m.provider_page_gateway,
		models: m.provider_page_models,
		advanced: m.provider_page_advanced
	};
	const title = $derived(
		m.provider_drawer_title({ provider: providerLabel(kind), account: account.name })
	);

	const moveTargets = $derived(
		accounts
			.filter(
				(a) =>
					a.id !== account.id &&
					a.user_id === account.user_id &&
					!a.providers.some((x) => x.family === p.family)
			)
			.map((a) => ({ id: a.id, name: a.name }))
	);

	async function move(targetId: string) {
		try {
			await actions.moveProvider(account.id, p.id, targetId);
			toasts.ok(m.accounts_provider_moved());
			onclose();
		} catch (e) {
			toasts.error(errMessage(e));
		}
	}

	async function save() {
		try {
			await actions.updateProvider(account.id, p.id, edit.body());
			toasts.ok(m.accounts_provider_updated());
			onclose();
		} catch (e) {
			toasts.error(errMessage(e));
		}
	}

	// The drawer keeps the width the user last dragged it to.
	const DRAWER_WIDTH_KEY = 'cctui_provider_drawer_width';
	const DRAWER_DEFAULT_PX = 620;
	const DRAWER_MIN_PX = 420;
	let width = $state(readWidth());
	let dragging = $state(false);
	let viewportWidth = $state(0);
	const maxPx = $derived(Math.max(DRAWER_MIN_PX, viewportWidth - 80));
	function readWidth(): number {
		if (typeof localStorage === 'undefined') return DRAWER_DEFAULT_PX;
		const n = Number(localStorage.getItem(DRAWER_WIDTH_KEY));
		return Number.isFinite(n) && n >= DRAWER_MIN_PX ? n : DRAWER_DEFAULT_PX;
	}
	function setWidth(px: number | undefined) {
		width = Math.max(DRAWER_MIN_PX, Math.round(px ?? DRAWER_DEFAULT_PX));
		localStorage.setItem(DRAWER_WIDTH_KEY, String(width));
	}
</script>

<svelte:window bind:innerWidth={viewportWidth} />

<Drawer
	side="right"
	width="{width}px"
	navWidth="150px"
	{title}
	page={LABELS[edit.page]()}
	{onclose}
	closeLabel={m.common_close()}
>
	{#snippet header()}
		<div class="head">
			<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
			<div
				class="grip"
				class:dragging
				role="separator"
				tabindex="0"
				aria-orientation="vertical"
				aria-valuemin={DRAWER_MIN_PX}
				aria-valuemax={maxPx}
				aria-label={m.dock_resize_grip()}
				title={m.dock_resize_grip()}
				use:resizeHandle={{
					side: 'right',
					min: DRAWER_MIN_PX,
					max: maxPx,
					onwidth: setWidth,
					onreset: () => setWidth(DRAWER_DEFAULT_PX),
					onactive: (a) => {
						dragging = a;
					}
				}}
			></div>
			<span class="mark"><AdapterIcon provider={kind} size={20} /></span>
			<div class="titles">
				<Text as="div" size="md" weight="semibold">{title}</Text>
				<Text as="div" size="xs" tone="faint">{LABELS[edit.page]()}</Text>
			</div>
			<span class="spacer"></span>
			<IconButton icon="x" label={m.common_close()} variant="ghost" onclick={onclose} />
		</div>
	{/snippet}

	{#snippet nav()}
		{#each pages as id (id)}
			<NavItem label={LABELS[id]()} active={edit.page === id} activeStyle="bar" onclick={() => (edit.page = id)} />
		{/each}
	{/snippet}

	<div class="pane">
		{#if edit.page === 'aliases'}
			<AliasesPage bind:rows={edit.aliasRows} models={edit.models} />
		{:else if edit.page === 'limits'}
			<LimitsPage
				rows={softRows}
				{windows}
				bind:edits={edit.soft}
				bind:rate={edit.rate}
			/>
			{#if kind === 'anthropic' || kind === 'openai'}
				<UsageNoticesEditor bind:value={edit.notices} />
			{/if}
		{:else if edit.page === 'models'}
			<ModelsPage
				fireworks={edit.isFireworks}
				bind:models={edit.models}
				bind:settings={edit.providerSettings}
			/>
		{:else}
			<SettingsPage
				groups={groupsOn(edit.page)}
				bind:settings={edit.settings}
				preset={catalog.data?.preset}
				loading={catalogLoading}
				failed={catalogFailed}
			>
				{#if edit.page === 'gateway'}
					{#if edit.isFireworks}
						<FireworksProviderEditor
							section="gateway"
							bind:settings={edit.providerSettings}
							bind:models={edit.models}
						/>
					{/if}
				{:else if edit.page === 'advanced'}
					<AdvancedPage
						endpoint={edit.isFireworks || edit.isCompatible}
						rawJson={edit.isAnthropic}
						{settableKeys}
						catalogReady={!!catalog.data}
						{claimed}
						bind:baseUrl={edit.baseUrl}
						bind:credential={edit.credential}
						bind:authScheme={edit.authScheme}
						bind:settings={edit.settings}
						{moveTargets}
						moveFamily={p.family}
						onmove={move}
					/>
				{/if}
			</SettingsPage>
		{/if}
	</div>

	{#snippet footer()}
		<Text as="span" size="xs" tone="faint">
			{changes === 0 ? m.provider_drawer_no_changes() : m.provider_drawer_changes({ n: changes })}
		</Text>
		<span class="spacer"></span>
		<Button size="sm" onclick={onclose}>{m.common_cancel()}</Button>
		<Button size="sm" variant="primary" onclick={save}>{m.common_save()}</Button>
	{/snippet}
</Drawer>

<style>
	/* A custom header lands in the panel grid's first cell (the nav column);
	   span it across the whole panel. */
	.head {
		grid-column: 1 / -1;
		display: flex;
		align-items: center;
		gap: var(--sp-3);
		padding: var(--sp-3) var(--sp-4);
		border-bottom: 1px solid var(--border);
	}
	/* The knob rides the panel's outer edge for the full height; the dialog is
	   in the top layer, so a fixed box lands on the viewport. */
	.grip {
		position: fixed;
		top: 0;
		bottom: 0;
		right: calc(var(--drawer-w) - 5px);
		width: 10px;
		cursor: ew-resize;
		touch-action: none;
		z-index: 1;
	}
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
	@media (max-width: 47.999rem) {
		.grip {
			display: none;
		}
	}
	.mark {
		display: inline-flex;
		flex: none;
	}
	.titles {
		display: flex;
		flex-direction: column;
		min-width: 0;
	}
	.spacer {
		flex: 1;
	}
	.pane {
		container-type: inline-size;
	}
</style>
