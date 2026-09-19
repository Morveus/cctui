<script lang="ts">
	// Conversation drawer header. Owns the title + rename, the secondary-action group
	// (rename · copy link · copy markdown · export · fork) which the kit Toolbar
	// collapses into a ⋯ menu on narrow bars, the interrupt/archive controls, and the
	// meta row (status badge, in-place codex model editor or the claude "fork to
	// change model" chip, machine badge, cwd, token usage). Action side-effects
	// are delegated to callbacks; the editing UI state lives here.
	import type { SessionListItem } from '@bindings/SessionListItem';
	import type { Label } from '@bindings/Label';
	import { statusBadgeTone } from '$lib/format';
	import { statusLabel } from '../sessioncard/view';
	import { sessionEnd, sessionEndTitle } from '$lib/sessionEnd';
	import { branchOf } from '../../../../routes/sessions/sessions.logic';
	import { fontScale, SCALE_LEVELS } from '$lib/fontscale.svelte';
	import { settings } from '$lib/settings.svelte';
	import { isArchiveChord } from '$lib/platform';
	import AdapterIcon from '$lib/components/atoms/AdapterIcon.svelte';
	import MachineBadge from '$lib/components/molecules/MachineBadge.svelte';
	import AccountBadge from '$lib/components/molecules/AccountBadge.svelte';
	import RebindTrail from '$lib/components/molecules/RebindTrail.svelte';
	import SessionDot from '$lib/components/molecules/SessionDot.svelte';
	import LabelBadge from '$lib/components/molecules/LabelBadge.svelte';
	import TokenUsage from '$lib/components/molecules/TokenUsage.svelte';
	import LangfuseChip from '$lib/components/molecules/LangfuseChip.svelte';
	import {
		Badge,
		Icon,
		IconButton,
		Input,
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
		onforkselect,
		forkSelectActive = false,
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
		// Toggle multi-select-to-fork mode; omitted → button hidden
		// (codex sessions have no partial-fork primitive).
		onforkselect?: () => void;
		forkSelectActive?: boolean;
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

	// Stand-ins for the `data-overflow` actions once the bar collapses.
	const overflowItems = $derived<MenuItem[]>([
		renaming
			? { label: m.common_save(), icon: 'check' as const, onselect: doRename }
			: { label: m.drawer_rename(), icon: 'edit' as const, onselect: startRename },
		{ label: m.drawer_copy_link_label(), icon: 'link' as const, onselect: oncopylink },
		{ label: m.drawer_copy_markdown_label(), icon: 'markdown' as const, onselect: oncopymarkdown },
		{ label: m.drawer_export_label(), icon: 'download' as const, onselect: onexport },
		{
			label: m.drawer_fork_label(),
			icon: 'fork' as const,
			pressed: onforkselect ? forkSelectActive : undefined,
			onselect: onforkselect ?? onfork
		}
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

<div class="dhead" data-journey="header">
	<Toolbar collapseBelow="640px" items={overflowItems} overflowLabel={m.drawer_more_actions()}>
		<IconButton icon="chevron-left" label={m.drawer_back()} onclick={onclose} />
		{#if onTogglePin}
			<span
				class="star"
				class:on={session.pinned}
				role="button"
				tabindex="0"
				title={session.pinned ? m.drawer_unpin_title() : m.drawer_pin_title()}
				aria-pressed={session.pinned}
				aria-label={session.pinned ? m.drawer_unpin_aria() : m.drawer_pin_aria()}
				onclick={() => onTogglePin?.(session)}
				onkeydown={(e: KeyboardEvent) => {
					if (e.key === 'Enter' || e.key === ' ') {
						e.preventDefault();
						onTogglePin?.(session);
					}
				}}>{session.pinned ? '★' : '☆'}</span
			>
		{/if}
		<SessionDot {session} {livenessClass} />
		<MachineBadge name={session.machine_name} id={session.machine_id} hue={session.machine_hue} mono />
		<AccountBadge name={session.account_name} onclick={onAccountClick} showName={settings.accountNames} />
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
		<FontScalePicker box="lg" />
		<!-- Secondary actions: inline on desktop, collapsed into the
		     ⋯ flyout on mobile so a long title + many buttons no longer overflow.
		     A single fork lives at the end of the group. -->
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
		<IconButton
			data-overflow
			chip
			variant="default"
			icon="link"
			label={m.drawer_copy_link_label()}
			title={m.drawer_copy_link_title()}
			onclick={oncopylink}
		/>
		<IconButton
			data-overflow
			chip
			variant="default"
			icon="markdown"
			label={m.drawer_copy_markdown_label()}
			title={m.drawer_copy_markdown_title()}
			onclick={oncopymarkdown}
		/>
		<IconButton
			data-overflow
			chip
			variant="default"
			icon="download"
			label={m.drawer_export_label()}
			title={m.drawer_export_title()}
			onclick={onexport}
		/>
		<IconButton
			data-overflow
			data-journey="fork"
			chip
			variant="default"
			icon="fork"
			label={m.drawer_fork_label()}
			title={onforkselect ? m.drawer_fork_select_title() : m.drawer_fork_title()}
			aria-pressed={onforkselect ? forkSelectActive : undefined}
			onclick={onforkselect ?? onfork}
		/>
		{#if !archived}
			<IconButton
				chip
				variant="default"
				tone="warn"
				style="background: color-mix(in srgb, var(--warn) 10%, var(--bg-elevated-2))"
				icon="archive"
				label={m.drawer_archive()}
				onclick={onarchive}
			/>
			<IconButton
				chip
				variant="default"
				tone="danger"
				style="background: color-mix(in srgb, var(--danger) 10%, var(--bg-elevated-2))"
				icon="stop"
				label={m.drawer_interrupt_label()}
				title={m.drawer_interrupt_title()}
				onclick={oninterrupt}
			/>
		{/if}
	</Toolbar>
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
	<div class="hmeta row row-wrap" data-journey="head-meta">
		{#if showStatusBadge}<Badge tone={statusBadgeTone(session.status)}>{statusLabel(session.status)}</Badge>{/if}
		{#if end}<Badge tone={end.tone} title={sessionEndTitle(end)} style={end.muted ? 'opacity:0.6' : undefined}>{end.label}</Badge>{/if}
		<WorkingDir path={session.working_dir} copy title={m.sessions_workdir_copy_title({ path: session.working_dir })} />
		{#if branch}
			<Badge mono title={m.sessions_branch_title({ branch })} style="display:inline-flex;align-items:center;gap:0.25em;min-width:0;max-width:14rem;flex:none">
				<Icon name="fork" size={12} label={m.sessions_branch_label()} />
				<span style="overflow:hidden;white-space:nowrap;text-overflow:ellipsis">{branch}</span>
			</Badge>
		{/if}
		<div class="meta-trail">
		<TokenUsage usage={session.token_usage} />
		<LangfuseChip id={session.id} />
		{#if isCodexSession && !archived}
			{#if modelEditing}
				<span class="model-edit">
					<Badge class="row" style="gap:var(--sp-1);padding:0.05rem var(--sp-1)">
						<ModelPicker
							id="drawer-model"
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
				<Badge
					as="button"
					mono
					title={m.drawer_change_model_title()}
					onclick={openModelEditor}
				>{session.model ?? m.drawer_default_model()}{session.effort ? ` · ${session.effort}` : ''} ✎</Badge>
			{/if}
		{:else if session.model || session.effort}
			<Badge
				as="button"
				mono
				title={m.drawer_no_inplace_model_title()}
				onclick={onfork}
			>{session.model ?? ''}{session.effort ? ` · ${session.effort}` : ''} ⑂</Badge>
		{/if}
		<AdapterIcon adapter={session.adapter_id} size={20} />
		</div>
	</div>
</div>

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
	.hmeta {
		gap: var(--sp-2);
		align-items: center;
	}
	/* Push token usage · model · logo to the right edge, opposite the working
	   dir, mirroring the session card footer. */
	.meta-trail {
		display: flex;
		align-items: center;
		gap: var(--sp-2);
		flex: none;
		margin-left: auto;
	}
	/* Star/pin toggle in the lead row (mirrors SessionCard). */
	.star {
		background: none;
		border: none;
		cursor: pointer;
		user-select: none;
		padding: 0;
		line-height: 1;
		font-size: 1.35rem;
		color: var(--text-faint);
		flex: none;
	}
	.star.on,
	.star:hover {
		color: var(--warn);
	}
	.model-edit {
		display: contents;
	}
</style>
