<script lang="ts">
	// Conversation drawer header. Owns the title + rename (collapsed into the ⋯
	// menu on narrow bars), the always-present ⋯ menu of less-used actions (copy
	// link, copy markdown, export, fork, terminal), the interrupt/archive controls, and the
	// meta row (status badge, in-place codex model editor or the claude "fork to
	// change model" chip, machine badge, cwd, token usage). Action side-effects
	// are delegated to callbacks; the editing UI state lives here.
	import type { SessionListItem } from '@bindings/SessionListItem';
	import type { Label } from '@bindings/Label';
	import { modelShort, statusBadgeTone } from '$lib/format';
	import { statusLabel } from '../sessioncard/view';
	import { sessionEnd, sessionEndTitle } from '$lib/sessionEnd';
	import { branchOf } from '../../../../routes/sessions/sessions.logic';
	import { fontScale, SCALE_LEVELS } from '$lib/fontscale.svelte';
	import { settings } from '$lib/settings.svelte';
	import { isArchiveChord } from '$lib/platform';
	import AdapterIcon from '$lib/components/atoms/AdapterIcon.svelte';
	import RebindTrail from '$lib/components/molecules/RebindTrail.svelte';
	import SessionGlyphs from '$lib/components/molecules/SessionGlyphs.svelte';
	import LabelBadge from '$lib/components/molecules/LabelBadge.svelte';
	import TokenUsage from '$lib/components/molecules/TokenUsage.svelte';
	import LangfuseChip from '$lib/components/molecules/LangfuseChip.svelte';
	import KeepaliveModal from '$lib/components/molecules/KeepaliveModal.svelte';
	import {
		Badge,
		Icon,
		IconButton,
		Input,
		Menu,
		Popover,
		Select,
		Text,
		Toolbar,
		WorkingDir,
		FontScalePicker,
		type MenuItem
	} from '@dorsk/tsumikit';
	import { codexModelsFor, codexEffortsFor, preferCatalog } from '$lib/harnessModels';
	import { useCodexModels, useMergedCodexModels } from '$lib/queries';
	import ModelPicker from '$lib/components/molecules/ModelPicker.svelte';
	import CodexModelsRefresh from '$lib/components/molecules/CodexModelsRefresh.svelte';
	import { m } from '$lib/paraglide/messages';

	let {
		session,
		archived,
		isCodexSession,
		livenessClass,
		showStatusBadge,
		onclose,
		onrename,
		onsetmodel,
		oncopylink,
		oncopymarkdown,
		onexport,
		onfork,
		onfollowup,
		onforkselect,
		forkSelectActive = false,
		onterminal,
		terminalOpen = false,
		oninterrupt,
		onarchive,
		onstoparchive,
		onTogglePin,
		// Opens the at-will account switcher from the key glyph; when omitted
		// the badge stays a read-only indicator.
		onAccountClick,
		// Label editing: same picker as the session card — when
		// `onAttachLabel` is supplied the strip is interactive, else read-only.
		allLabels = [],
		onCreateLabel,
		onAttachLabel,
		onDetachLabel,
		onUpdateLabel,
		onDeleteLabel
	}: {
		session: SessionListItem;
		archived: boolean;
		isCodexSession: boolean;
		livenessClass: string;
		showStatusBadge: boolean;
		onclose: () => void;
		onrename: (name: string) => void;
		onsetmodel: (model: string, effort: string) => void;
		oncopylink: () => void;
		oncopymarkdown: () => void;
		onexport: () => void;
		onfork: () => void;
		onfollowup?: () => void;
		// Toggle multi-select-to-fork mode; omitted → button hidden
		// (codex sessions have no partial-fork primitive).
		onforkselect?: () => void;
		forkSelectActive?: boolean;
		/** Toggles the read-only live terminal; omit to hide the entry. */
		onterminal?: () => void;
		terminalOpen?: boolean;
		oninterrupt: () => void;
		onarchive: () => void;
		// Stop-then-archive, fired by the ⌘/Ctrl+E keyboard chord.
		onstoparchive: () => void;
		onTogglePin?: (s: SessionListItem) => void;
		onAccountClick?: () => void;
		allLabels?: Label[];
		onCreateLabel?: (name: string, color: string) => Promise<Label>;
		onAttachLabel?: (id: string, labelId: string) => void | Promise<void>;
		onDetachLabel?: (id: string, labelId: string) => void | Promise<void>;
		onUpdateLabel?: (labelId: string, patch: { name?: string; color?: string }) => Promise<Label>;
		onDeleteLabel?: (labelId: string) => void | Promise<void>;
	} = $props();

	const end = $derived(sessionEnd(session));
	const branch = $derived(branchOf(session));

	// Label picker is interactive only when an attach handler is wired in.
	const labelEditable = $derived(!!onAttachLabel);

	const headTitle = $derived(session.name || session.working_dir);

	let renaming = $state(false);
	// svelte-ignore state_referenced_locally
	let newName = $state(session.name ?? '');
	// In-place model/effort editor, codex only.
	let modelEditing = $state(false);
	let pendingModel = $state('');
	let pendingEffort = $state('');

	// Codex catalog, fetched only while the editor is open: the session
	// machine's own report, else the cross-machine merge, else the static list.
	const machineCodexCatalog = useCodexModels(() =>
		isCodexSession && modelEditing ? session.machine_id : ''
	);
	const mergedCodexCatalog = useMergedCodexModels(() => isCodexSession && modelEditing);
	const codexCatalog = $derived(preferCatalog(machineCodexCatalog.data, mergedCodexCatalog.data));
	const codexModelOptions = $derived(codexModelsFor(codexCatalog));
	const codexEffortOptions = $derived(codexEffortsFor(codexCatalog, pendingModel));

	function startRename() {
		renaming = true;
		newName = session.name ?? '';
	}
	function doRename() {
		const n = newName.trim();
		renaming = false;
		if (!n) return;
		onrename(n);
	}
	function openModelEditor() {
		pendingModel = session.model ?? '';
		pendingEffort = session.effort ?? '';
		modelEditing = true;
	}
	function applyModelChange() {
		const model = pendingModel.trim();
		const effort = pendingEffort.trim();
		modelEditing = false;
		if (!model && !effort) return;
		onsetmodel(model, effort);
	}

	let keepaliveOpen = $state(false);

	const followupItem = $derived<MenuItem | null>(
		onfollowup ? { label: m.drawer_followup_label(), icon: 'arrow-right', onselect: onfollowup } : null
	);

	// Mirrors the Toolbar's `collapseBelow`: the rename stand-in joins the menu
	// only while its inline button is hidden.
	const COLLAPSE_BELOW = 640;
	let barWidth = $state(Infinity);
	const collapsed = $derived(barWidth < COLLAPSE_BELOW);

	const hasModelMeta = $derived((isCodexSession && !archived) || !!session.model || !!session.effort);

	const overflowItems = $derived<MenuItem[]>([
		...(collapsed
			? [
					renaming
						? { label: m.common_save(), icon: 'check' as const, onselect: doRename }
						: { label: m.drawer_rename(), icon: 'edit' as const, onselect: startRename }
				]
			: []),
		{
			label: m.drawer_copy_link_label(),
			icon: 'link' as const,
			attrs: { title: m.drawer_copy_link_title() },
			onselect: oncopylink
		},
		{
			label: m.drawer_copy_markdown_label(),
			icon: 'markdown' as const,
			attrs: { title: m.drawer_copy_markdown_title() },
			onselect: oncopymarkdown
		},
		{
			label: m.drawer_export_label(),
			icon: 'download' as const,
			attrs: { title: m.drawer_export_title() },
			onselect: onexport
		},
		...(followupItem && settings.preferFollowupOverFork ? [followupItem] : []),
		{
			label: m.drawer_fork_label(),
			icon: 'fork' as const,
			pressed: onforkselect ? forkSelectActive : undefined,
			attrs: { title: onforkselect ? m.drawer_fork_select_title() : m.drawer_fork_title() },
			onselect: onforkselect ?? onfork
		},
		...(onterminal
			? [
					{
						label: m.drawer_terminal_label(),
						icon: 'tv' as const,
						pressed: terminalOpen,
						attrs: { title: m.conversation_terminal_title(), 'data-journey': 'terminal' },
						onselect: onterminal
					}
				]
			: []),
		{
			label: m.drawer_keepalive_label(),
			icon: 'recycle' as const,
			pressed: !!session.keepalive,
			onselect: () => (keepaliveOpen = true)
		},
		...(followupItem && !settings.preferFollowupOverFork ? [followupItem] : [])
	]);

	function onWinKey(e: KeyboardEvent) {
		// Archive chord (⌘ E / Ctrl+E): interrupt any running turn and archive the
		// session, which then dismisses the drawer. Opt-out via Settings. Skipped
		// while renaming (so the chord can't fire mid-edit) and on already-archived
		// sessions (nothing to archive). Window-level so it works regardless of
		// whether focus is in the composer.
		if (!archived && !renaming && settings.archiveShortcut && isArchiveChord(e)) {
			e.preventDefault();
			onstoparchive();
			return;
		}
		if (e.key !== 'Escape' || renaming) return;
		onclose();
	}
</script>

<svelte:window onkeydown={onWinKey} />

{#snippet modelText(model: string)}
	<span class="ellipsis"
		><span class="m-full">{model}</span><span class="m-short">{modelShort(model)}</span
		>{#if session.effort}<span class="m-effort"> · {session.effort}</span>{/if}</span
	>
{/snippet}

{#snippet modelMeta(idPrefix: string)}
	{#if isCodexSession && !archived}
		{#if modelEditing}
			<span class="model-edit">
				<Badge class="row" style="gap:var(--sp-1);padding:0.05rem var(--sp-1)">
					<ModelPicker
						id="{idPrefix}-model"
						compact
						variant="embedded"
						width="auto"
						bind:value={pendingModel}
						options={codexModelOptions}
						aria-label={m.drawer_model_aria()}
					/>
					<CodexModelsRefresh machineId={session.machine_id} size={14} />
					<Select
						variant="embedded"
						width="auto"
						size="sm"
						chevron={false}
						bind:value={pendingEffort}
						aria-label={m.drawer_effort_aria()}
					>
						{#each codexEffortOptions as e (e)}<option value={e}>{e || m.drawer_default_effort()}</option>{/each}
					</Select>
					<IconButton chip variant="default" icon="check" label={m.common_apply()} onclick={applyModelChange} />
					<IconButton chip variant="default" icon="x" label={m.common_cancel()} onclick={() => (modelEditing = false)} />
				</Badge>
			</span>
		{:else}
			<span class="model">
				<Badge
					as="button"
					mono
					title={m.drawer_change_model_title()}
					onclick={openModelEditor}
					style="min-width:0;max-width:100%"
					>{@render modelText(session.model ?? m.drawer_default_model())} ✎</Badge
				>
			</span>
		{/if}
	{:else if session.model || session.effort}
		<span class="model">
			<Badge
				as="button"
				mono
				title={m.drawer_no_inplace_model_title()}
				onclick={onfork}
				style="min-width:0;max-width:100%"
				>{@render modelText(session.model ?? '')} ⑂</Badge
			>
		</span>
	{/if}
{/snippet}

<div class="dhead" data-journey="header">
	<div class="dbar" bind:clientWidth={barWidth}>
	<Toolbar collapseBelow="{COLLAPSE_BELOW}px" density={collapsed ? 'compact' : 'default'}>
		<IconButton icon="chevron-left" label={m.drawer_back()} box={collapsed ? 'sm' : 'md'} onclick={onclose} />
		<SessionGlyphs
			{session}
			{livenessClass}
			stack={collapsed ? 'always' : 'never'}
			showAccountName={settings.accountNames}
			{onTogglePin}
			{onAccountClick}
		/>
		<RebindTrail sessionId={session.id} />
		<div class="dtitle">
			{#if renaming}
				<Input
					bind:value={newName}
					aria-label={m.a11y_rename_session()}
					onsubmit={doRename}
				/>
			{:else}
				<Text weight="semibold" size="md" truncate>{headTitle}</Text>
				{#if session.labels.length === 0 && labelEditable}
					<!-- No labels yet: the tag picker rides inline right after the title
					     text rather than claiming an empty full-width row. Once labels
					     exist the strip moves to its own row below (see .hlabels). -->
					<LabelBadge
						labels={[]}
						editable
						{allLabels}
						onCreate={onCreateLabel}
						onAttach={(lid) => onAttachLabel?.(session.id, lid)}
						onDetach={(lid) => onDetachLabel?.(session.id, lid)}
						onUpdate={onUpdateLabel}
						onDelete={onDeleteLabel}
					/>
				{/if}
			{/if}
		</div>
		<!-- Text size: the same kit picker as the main header, writing the one
		     global fontScale. It stays out of the ⋯ flyout on mobile. -->
		<FontScalePicker box={collapsed ? 'sm' : 'lg'} />
		{#if renaming}
			<IconButton data-overflow chip variant="default" icon="check" label={m.common_save()} onclick={doRename} />
		{:else}
			<IconButton
				data-overflow
				chip
				variant="default"
				icon="edit"
				label={m.drawer_rename()}
				onclick={startRename}
			/>
		{/if}
		{#if !archived}
			<IconButton
				chip={!collapsed}
				variant="default"
				tone="warn"
				box={collapsed ? 'sm' : 'md'}
				style="background: color-mix(in srgb, var(--warn) 10%, var(--bg-elevated-2))"
				icon="archive"
				label={m.drawer_archive()}
				onclick={onarchive}
			/>
			<IconButton
				chip={!collapsed}
				variant="default"
				tone="danger"
				box={collapsed ? 'sm' : 'md'}
				style="background: color-mix(in srgb, var(--danger) 10%, var(--bg-elevated-2))"
				icon="stop"
				label={m.drawer_interrupt_label()}
				title={m.drawer_interrupt_title()}
				onclick={oninterrupt}
			/>
		{/if}
		<Menu label={m.drawer_more_actions()} items={overflowItems} placement="bottom-end" box="sm">
			{#snippet trigger()}
				<IconButton data-journey="actions" icon="more" label={m.drawer_more_actions()} box="sm" />
			{/snippet}
		</Menu>
	</Toolbar>
	</div>
	{#if session.labels.length > 0}
		<!-- Labels get their own full-width row in the header's column stack, so the
		     strip can spread edge-to-edge and wrap freely instead of being boxed
		     into the title row's leftover width (under the action buttons). The
		     empty-state trigger lives inline by the title (above), so this row only
		     appears once there's at least one label. -->
		<div class="hlabels">
			<LabelBadge
				labels={session.labels}
				editable={labelEditable}
				{allLabels}
				onCreate={onCreateLabel}
				onAttach={(lid) => onAttachLabel?.(session.id, lid)}
				onDetach={(lid) => onDetachLabel?.(session.id, lid)}
				onUpdate={onUpdateLabel}
				onDelete={onDeleteLabel}
			/>
		</div>
	{/if}
	<div class="hmeta" class:editing={modelEditing} data-journey="head-meta">
		{#if showStatusBadge}<Badge tone={statusBadgeTone(session.status)}>{statusLabel(session.status)}</Badge>{/if}
		{#if end}<Badge tone={end.tone} title={sessionEndTitle(end)} style={end.muted ? 'opacity:0.6' : undefined}>{end.label}</Badge>{/if}
		<span class="cwd">
			<WorkingDir
				path={session.working_dir}
				copy
				shrink
				title={m.sessions_workdir_copy_title({ path: session.working_dir })}
				style="min-width:min(9rem,40%)"
			/>
		</span>
		{#if branch}
			<span class="branch">
				<Badge mono title={m.sessions_branch_title({ branch })} style="display:inline-flex;align-items:center;gap:0.25em;min-width:0;max-width:100%">
					<Icon name="fork" size={12} label={m.sessions_branch_label()} />
					<span class="ellipsis">{branch}</span>
				</Badge>
			</span>
		{/if}
		<div class="meta-trail">
		<TokenUsage usage={session.token_usage} />
		<span class="langfuse"><LangfuseChip id={session.id} /></span>
		{@render modelMeta('drawer')}
		<AdapterIcon adapter={session.adapter_id} size={20} />
		{#if hasModelMeta}
			<span class="meta-details">
				<Popover
					label={m.drawer_meta_details()}
					placement="bottom-end"
					box="sm"
					data-journey="head-details"
				>
					{#snippet trigger()}<Icon name="info" size={16} />{/snippet}
					<div class="metapop">
						<span class="langfuse"><LangfuseChip id={session.id} /></span>
						{@render modelMeta('drawer-details')}
					</div>
				</Popover>
			</span>
		{/if}
		</div>
	</div>
</div>

{#if keepaliveOpen}
	<KeepaliveModal {session} onclose={() => (keepaliveOpen = false)} />
{/if}

<style>
	.dhead {
		position: sticky;
		top: 0;
		z-index: 2;
		display: flex;
		flex-direction: column;
		gap: var(--sp-2);
		padding: var(--sp-2) var(--sp-3);
		border-bottom: 1px solid var(--border);
		background: var(--bg-elevated);
		/* TokenUsage degrades its readout against this container. */
		container: drawer-head / inline-size;
	}
	.dbar {
		min-width: 0;
	}
	/* Labels on their own row so the strip spans the full header width. */
	.hlabels {
		display: flex;
		min-width: 0;
	}
	.dtitle {
		flex: 1;
		min-width: 0;
		display: flex;
		align-items: center;
		gap: var(--sp-1);
	}
	/* One row: the model gives way first, then the branch; the cwd holds out
	   longest. */
	.hmeta {
		display: flex;
		align-items: center;
		gap: var(--sp-2);
		min-width: 0;
	}
	.hmeta.editing {
		flex-wrap: wrap;
	}
	.cwd {
		display: contents;
	}
	.branch {
		display: inline-flex;
		flex: 0 3 auto;
		min-width: 4.5rem;
		max-width: 14rem;
	}
	.ellipsis {
		min-width: 0;
		overflow: hidden;
		white-space: nowrap;
		text-overflow: ellipsis;
	}
	.meta-trail {
		display: flex;
		align-items: center;
		gap: var(--sp-2);
		flex: 0 8 auto;
		min-width: 0;
		margin-left: auto;
	}
	.model {
		display: inline-flex;
		flex: 0 1 auto;
		min-width: 0;
	}
	.langfuse {
		display: contents;
	}
	.m-short {
		display: none;
	}
	.model-edit {
		display: contents;
	}
	/* The ⓘ details trigger exists only to carry what the row has dropped, so it
	   appears exactly when the first item goes. */
	.meta-details {
		display: none;
	}
	@container drawer-head (max-width: 40rem) {
		.branch {
			max-width: 8rem;
		}
		.langfuse,
		.m-effort,
		.m-full {
			display: none;
		}
		.m-short {
			display: inline;
		}
		.meta-details {
			display: inline-flex;
		}
	}
	@container drawer-head (max-width: 26rem) {
		.model,
		.model-edit {
			display: none;
		}
	}
	/* The popover holds what the row dropped, so it always shows the full model
	   text the narrow row degrades. */
	.metapop {
		display: flex;
		flex-direction: column;
		align-items: flex-start;
		gap: var(--sp-2);
		min-width: 0;
		max-width: 100%;
	}
	.metapop .m-full,
	.metapop .m-effort {
		display: inline;
	}
	.metapop .m-short {
		display: none;
	}
	.metapop .langfuse,
	.metapop .model-edit {
		display: contents;
	}
	.metapop .model {
		display: inline-flex;
	}
</style>
