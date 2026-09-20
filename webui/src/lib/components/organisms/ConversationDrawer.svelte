<script lang="ts">
	import type { SessionListItem } from '@bindings/SessionListItem';
	import type { AgentEvent } from '@bindings/AgentEvent';
	import { page } from '$app/state';
	import { replaceState } from '$app/navigation';
	import { hrefWithoutDiagnose } from '../../../routes/sessions/sessions.logic';
	import { ws } from '$lib/ws.svelte';
	import {
		useConversation,
		useSessionActions,
		useLabels,
		useAccounts,
		qk,
		endpoints,
		CONVERSATION_FETCH_LIMIT,
		useMessagePins,
		useMessagePinActions
	} from '$lib/queries';
	import { useQueryClient } from '@tanstack/svelte-query';
	import { renderMarkdown, highlightBlock } from '$lib/markdown';
	const isMachineUuid = (v: string) =>
		/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(v);
	import { highlightTerms } from '$lib/search';
	import { drafts, VIEW_OPTS } from '$lib/drafts';
	import { Button, Dropzone, ResizablePanel } from '@dorsk/tsumikit';
	import ForkModal from './conversation/ForkModal.svelte';
	import DrawerHeader from './conversation/DrawerHeader.svelte';
	import DrawerToolbar from './conversation/DrawerToolbar.svelte';
	import DiagnosePanel from './conversation/DiagnosePanel.svelte';
	import ActivityBanner from './conversation/ActivityBanner.svelte';
	import TaskPanel from './conversation/TaskPanel.svelte';
	import TerminalPane from './conversation/TerminalPane.svelte';
	import Conversation from './conversation/Conversation.svelte';
	import AccountSwitchModal from './conversation/AccountSwitchModal.svelte';
	import ConversationComposer from './conversation/ConversationComposer.svelte';
	import QueuedBanner from './conversation/QueuedBanner.svelte';
	import BookmarkSaveModal from './bookmarks/BookmarkSaveModal.svelte';
	import type { Line, MsgCategory, ViewOpts } from './conversation/types';
	import { parseViewOpts } from './conversation/filters';
	import { mergeEventSources } from './conversation/format';
	import { buildLines, type LineBuildCtx } from './conversation/lines';
	import { ConversationStream, mergeLiveEvent } from './conversation/stream.svelte';
	import { ScrollController } from './conversation/scroll.svelte';
	import { createSeqJumper, type RenderWindow } from './conversation/jump';
	import { SearchHitStepper } from './conversation/searchHits.svelte';
	import { ForkController } from './conversation/fork.svelte';
	import { SessionActions } from './conversation/sessionActions.svelte';
	import { draftFromLine, isLineBookmarked } from '$lib/bookmarks';
	import type { CreateBookmark } from '@bindings/CreateBookmark';
	import { useBookmarkActions, useBookmarks } from '$lib/queries';
	import { toasts } from '$lib/toast.svelte';
	import { errMessage } from '$lib/api';
	import { m } from '$lib/paraglide/messages';

	let {
		session,
		onclose,
		highlight = [],
		focusSeq = null,
		onNewFromScript,
		onNavigate
	}: {
		session: SessionListItem;
		onclose: () => void;
		highlight?: string[];
		/** Causal seq of the matched message when opened from a search hit
		 *  (`SessionListItem.match_seq`). `null` opens tail-anchored as usual. */
		focusSeq?: number | null;
		// "New session from same script" for archived sessions.
		onNewFromScript?: (s: SessionListItem) => void;
		// Open another session in place by id — used to jump straight to a
		// freshly forked conversation without a manual refresh.
		onNavigate?: (sessionId: string) => void;
	} = $props();

	// Search terms to highlight inline, set when opened from a search.
	const hl = (html: string) => (highlight.length ? highlightTerms(html, highlight) : html);

	const id = $derived(session.id);
	const archived = $derived(session.status === 'archived');
	// Held back by its machine's RAM ceiling: nothing runs yet, so the drawer
	// shows why and the overrides instead of a conversation and a composer.
	const queued = $derived(session.status === 'queued');
	const needsInput = $derived(session.attention === 'needs_input' && !archived);
	// Liveness dot next to the title, mirroring SessionCard.
	const livenessClass = $derived(
		session.hibernated
			? 'dot-hibernated'
			: session.liveness === 'active'
				? 'dot-active'
				: session.liveness === 'stale'
					? 'dot-stale'
					: 'dot-dead'
	);
	const showStatusBadge = $derived(
		session.status === 'new' || session.status === 'archived' || session.status === 'queued'
	);
	const qc = useQueryClient();

	// Session diagnose panel, opened from the toolbar or a failure toast's
	// Diagnose action (`?diagnose=1`).
	let diagnoseOpen = $state(false);
	// Read-only live terminal pane, toggled from the toolbar.
	let terminalOpen = $state(false);
	// A navigation to another session must not leave a stale panel open.
	$effect(() => {
		void id;
		diagnoseOpen = page.url.searchParams.get('diagnose') === '1';
		terminalOpen = false;
	});

	function closeDiagnose() {
		diagnoseOpen = false;
		const href = hrefWithoutDiagnose(location.href);
		if (href) replaceState(href, page.state);
	}

	const DRAWER_MIN_PX = 360;
	const DRAWER_DEFAULT_PX = 900;
	let view = $state<ViewOpts>(parseViewOpts(drafts.get(VIEW_OPTS)));
	const visible = (c: MsgCategory): boolean => view.msgFilter[c];
	$effect(() => {
		drafts.set(VIEW_OPTS, JSON.stringify(view));
	});

	const history = useConversation(
		() => id,
		() => true
	);
	const actions = useSessionActions();

	// Labels + pin in the drawer header — the same global
	// label set and mutations the session list uses, so editing a session's
	// labels/star from the open conversation stays in sync with the list.
	const labelsQuery = useLabels();
	const allLabels = $derived(labelsQuery.data?.labels ?? []);
	const createLabel = (name: string, color: string) => actions.createLabel(name, color);
	const attachLabel = (sid: string, labelId: string) => actions.attachLabel(sid, labelId);
	const detachLabel = (sid: string, labelId: string) => actions.detachLabel(sid, labelId);
	const updateLabel = (labelId: string, patch: { name?: string; color?: string }) =>
		actions.updateLabel(labelId, patch);
	const deleteLabel = (labelId: string) => actions.deleteLabel(labelId);
	const togglePin = (s: SessionListItem) => (s.pinned ? actions.unpin(s.id) : actions.pin(s.id));

	// ── Sticky-bottom scroll controller ──────────────────────────
	// Shared by the viewport (binds the scroller) and the composer (binds the
	// textarea, whose growth must re-pin the viewport).
	const scroll = new ScrollController();

	// ── WS subscription + live events + send orchestration ──────────────────
	// Owns the live buffer, optimistic echoes, permission/ask/delivery state and
	// the "Working…" activity indicator (see conversation/stream.svelte.ts).
	const stream = new ConversationStream({
		id: () => id,
		archived: () => archived,
		historyData: () => history.data,
		pin: scroll.stickToBottom,
		invalidateConversation: () => qc.invalidateQueries({ queryKey: qk.conversation(id) }),
		invalidateSessions: () => qc.invalidateQueries({ queryKey: ['sessions'] }),
		mergeIntoCache: (sid, ev) =>
			qc.setQueryData<AgentEvent[]>(qk.conversation(sid), (prev) => mergeLiveEvent(prev, ev))
	});
	// (Re)subscribe when the open session changes or a forced resubscribe is
	// requested; tear down listeners on switch/unmount.
	$effect(() => {
		const sid = id;
		void stream.resubTick;
		return stream.subscribe(sid);
	});
	// Catch up after the tab regains focus (the ws may have gone half-open).
	$effect(() => stream.installVisibilityRefresh());

	// At-will account switcher: opened from the header key glyph, or
	// auto-opened when a soft limit blocks the chat. Accounts are
	// fetched lazily — only while the modal is open or a block is active, so the
	// common case pays nothing.
	let acctModalOpen = $state(false);
	const accounts = useAccounts(() => acctModalOpen || stream.softLimit !== null);

	// Auto-open the switcher the first time a given soft-limit block lands, so the
	// stalled chat surfaces a way out without the user hunting for the key glyph.
	let lastSoftLimitId = $state<string | null>(null);
	$effect(() => {
		const sl = stream.softLimit;
		if (sl && sl.account_id !== lastSoftLimitId) {
			lastSoftLimitId = sl.account_id;
			acctModalOpen = true;
		} else if (!sl) {
			lastSoftLimitId = null;
		}
	});

	// History (fetched) + live (ws) events, merged and ordered by causal `seq`
	// (falling back to `ts`) via `orderEvents`. Live events already present in
	// history are dropped so a reconnect/focus refetch and the persisted form
	// of an optimistic reply don't render twice. Ordering by the server's
	// insert `seq` keeps a reloaded AskUserQuestion above its answer (which a
	// `ts`-only sort inverted) while an optimistic reply that survives a
	// refetch still lands in its correct place.
	// Older pages fetched via the `before` cursor; the query cache only ever
	// holds the newest CONVERSATION_FETCH_LIMIT events, so refetches stay small.
	let earlier = $state<AgentEvent[]>([]);
	let earlierExhausted = $state(false);
	let fetchingEarlier = $state(false);
	$effect(() => {
		void id;
		earlier = [];
		earlierExhausted = false;
	});
	const canFetchEarlier = $derived(
		!earlierExhausted &&
			((history.data?.length ?? 0) >= CONVERSATION_FETCH_LIMIT || earlier.length > 0)
	);

	const events = $derived(mergeEventSources(history.data ?? [], earlier, stream.live));

	async function fetchEarlier() {
		if (fetchingEarlier || earlierExhausted) return;
		const oldest = events.find((e) => typeof e.seq === 'number')?.seq;
		if (oldest == null) return;
		const sid = id;
		fetchingEarlier = true;
		try {
			const page = await endpoints.conversation(sid, {
				limit: CONVERSATION_FETCH_LIMIT,
				before: oldest
			});
			if (sid !== id) return;
			if (page.length < CONVERSATION_FETCH_LIMIT) earlierExhausted = true;
			earlier = [...page, ...earlier];
		} finally {
			fetchingEarlier = false;
		}
	}

	// ── Search focus: open centred on the hit instead of the tail ───────────
	// One extra window fetch, prepended into `earlier`. The cached tail query is
	// left alone so live events, dedup and jump-to-bottom keep working; on a
	// session longer than both windows the two are not contiguous, and the
	// jump-to-bottom pill is the bridge.
	const FOCUS_CONTEXT = 40;
	let focusFetched = $state<string | null>(null);
	$effect(() => {
		const seq = focusSeq;
		const sid = id;
		if (seq == null) return;
		const token = `${sid}|${seq}`;
		if (focusFetched === token) return;
		focusFetched = token;
		void (async () => {
			const win = await endpoints.conversation(sid, {
				limit: FOCUS_CONTEXT * 2,
				before: seq + FOCUS_CONTEXT
			});
			if (sid !== id) return;
			// A short window really is the head of the transcript: `before` is an
			// absolute cursor, not a page number.
			if (win.length < FOCUS_CONTEXT * 2) earlierExhausted = true;
			earlier = [...win, ...earlier];
		})();
	});

	// `Line` carries no seq, so the focused line is addressed by `ts`. A pruned
	// or filtered-out event resolves to null and the drawer just opens normally.
	const focusTs = $derived.by(() => {
		if (focusSeq == null) return null;
		const ev = events.find((e) => e.seq === focusSeq);
		return ev ? Number(ev.ts) : null;
	});

	// ── Line building (parse + filter + dedup + delivery tinting) ───────────
	// Render markdown honoring the table formatting toggle. Local file paths
	// link to the machine read-file route only when `machine_id` is the machine
	// UUID (daemon sessions); legacy hostname-valued rows keep paths as text.
	const mdRender = (s: string) =>
		hl(
			renderMarkdown(s, {
				tables: view.prettyTables,
				sessionId: id,
				machineId: isMachineUuid(session.machine_id) ? session.machine_id : undefined
			})
		);
	// Getters, not snapshots: the toggles are read at build time so the derived
	// below re-runs when they flip.
	const lineCtx: LineBuildCtx = {
		visible,
		renderMarkdown: mdRender,
		renderCode: (text, lang) => hl(highlightBlock(text, lang)),
		get prettyJson() {
			return view.prettyJson;
		},
		get prettyDiff() {
			return view.prettyDiff;
		}
	};
	const lines = $derived.by(() =>
		buildLines(events, lineCtx, {
			pending: stream.pendingReplies,
			failed: stream.failedReplies,
			retrying: stream.retryingReplies
		})
	);
	// The assistant prose preceding the live question, rendered as
	// markdown above the card so the user answers with context, not blind.
	const askPreambleHtml = $derived(
		stream.ask?.preamble ? hl(renderMarkdown(stream.ask.preamble)) : null
	);
	// The assistant prose preceding the live plan, same treatment.
	const planPreambleHtml = $derived(
		stream.plan?.preamble ? hl(renderMarkdown(stream.plan.preamble)) : null
	);

	// ── Scroll wiring (content-follow, session reset, composer-growth observer) ─
	// Only follow new content when the user is pinned to the bottom.
	$effect(() => {
		void lines.length;
		void stream.perms.length;
		void stream.working;
		scroll.followIfStuck();
	});
	// Reset to bottom + sticky when switching sessions — except when opened on a
	// search hit, which must land mid-transcript and stay there.
	$effect(() => {
		void id;
		if (focusSeq == null) scroll.resetForSession();
		else scroll.unstick();
	});
	// Keep pinned to the bottom while the composer grows. Re-runs when
	// the scroller / textarea attach (the controller reads both reactively).
	$effect(() => scroll.observeResize());

	// ── Message pins + the shared jump primitive ───────────────────────────
	const pinsQuery = useMessagePins(() => id);
	const pins = $derived(pinsQuery.data ?? []);
	const pinnedSeqs = $derived(new Set(pins.map((p) => p.seq)));
	const pinActions = useMessagePinActions();
	let renderWindow = $state<RenderWindow | undefined>(undefined);
	const { ensureSeqVisible } = createSeqJumper({
		hasSeq: (seq) => events.some((e) => e.seq === seq),
		isRendered: (seq) => renderWindow?.isRendered(seq) ?? false,
		growRender: () => renderWindow?.grow(),
		canFetchOlder: () => canFetchEarlier,
		fetchOlder: fetchEarlier,
		centerOnSeq: scroll.centerOnSeq,
		unstick: scroll.unstick
	});
	function togglePinLine(ln: Line) {
		if (ln.seq === undefined || ln.pending || ln.failed) return;
		void (pinnedSeqs.has(ln.seq)
			? pinActions.unpin(id, ln.seq)
			: pinActions.pin(id, ln.seq, ln.messageId ?? null));
	}
	// ── Search hit stepping ─────────────────────────────────────────────────
	let conv = $state<Conversation>();
	const hits = new SearchHitStepper({
		scroller: () => scroll.scroller,
		loadOlder: () => conv?.loadOlder()
	});
	$effect(() => {
		void lines.length;
		void highlight;
		hits.refresh();
	});
	$effect(() => {
		void id;
		hits.reset();
	});

	const isCodexSession = $derived((session.adapter_id ?? '').startsWith('codex'));

	// ── Session actions (rename / archive / interrupt / resume / model switch /
	// auto-approve / export / copy) ─────────────────────────────────────────
	// Thin wrappers over the query layer + export helpers, collected into one
	// controller so the drawer stays a composition shell and the header/composer
	// stay presentational (conversation/sessionActions.svelte.ts).
	const sa = new SessionActions({
		id: () => id,
		session: () => session,
		events: () => events,
		view: () => view,
		actions,
		onclose: () => onclose()
	});

	// ── Fork conversation ───────────────────────────────────────────
	// Self-contained hook (conversation/fork.svelte.ts): also the supported
	// "switch model" substitute for claude and the "reopen" path for
	// archived sessions.
	const fork = new ForkController({
		id: () => id,
		archived: () => archived,
		isCodex: () => isCodexSession,
		session: () => session,
		fork: (sid, body) => actions.fork(sid, body),
		// Jump straight to the new conversation when claude returned its id;
		// otherwise close and let the list refetch surface it.
		onForked: (sid) => {
			if (sid && onNavigate) onNavigate(sid);
			else onclose();
		}
	});

	// Subset fork from a conversation extract. Claude-only; codex has
	// no partial-fork primitive, so the per-message actions are gated off for it.
	const forkable = $derived(!isCodexSession && !archived);
	let selectMode = $state(false);
	let selected = $state<Set<string>>(new Set());
	function toggleSelect(messageId: string) {
		const next = new Set(selected);
		if (next.has(messageId)) next.delete(messageId);
		else next.add(messageId);
		selected = next;
	}
	function exitSelect() {
		selectMode = false;
		selected = new Set();
	}
	// Forkable assistant anchors in render order: drives the from/to
	// range picker — the fork spans the checked endpoints inclusively.
	const forkableIds = $derived(
		lines
			.filter((l) => l.role === 'assistant' && l.messageId)
			.map((l) => l.messageId as string)
	);
	function forkSelection() {
		if (selected.size === 0) return;
		// Fork the contiguous span between the first and last checked message: a
		// single check forks just that message; checking a *from* and a *to* forks
		// everything between them.
		const idxs = [...selected]
			.map((mid) => forkableIds.indexOf(mid))
			.filter((i) => i >= 0)
			.sort((a, b) => a - b);
		if (idxs.length === 0) return;
		const range = forkableIds.slice(idxs[0], idxs[idxs.length - 1] + 1);
		fork.openExtract({
			mode: 'selected',
			anchor_message_id: null,
			selected_message_ids: range
		});
	}

	// Mid-chat file attachments are supported on filesystem-backed adapters
	//; the composer owns the attachment state, the viewport's dropzone
	// feeds it via the component ref.
	const supportsAttachments = $derived(
		session.adapter_id === 'claude-code' || session.adapter_id === 'codex'
	);
	let composer = $state<ConversationComposer>();

	// Edit a still-pending message: drop the in-flight echo and pull its
	// text back into the composer to fix and resend.
	function editPending(text: string, ts: number) {
		if (archived) return;
		stream.discardOptimistic(ts);
		composer?.loadDraft(text);
	}

	// ── Bookmarks (CCT-992) ────────────────────────────────
	const savedBookmarks = useBookmarks();
	const bookmarkActions = useBookmarkActions();
	let bookmarkDraft = $state<CreateBookmark | null>(null);

	const isBookmarked = (ln: Line) =>
		isLineBookmarked(savedBookmarks.data ?? [], id, ln) !== null;

	async function saveBookmark(title: string, note: string | null) {
		const draft = bookmarkDraft;
		bookmarkDraft = null;
		if (!draft) return;
		try {
			await bookmarkActions.create({ ...draft, title, note });
			toasts.ok(m.bookmarks_saved());
		} catch (e) {
			toasts.error(m.bookmarks_save_failed({ message: errMessage(e) }));
		}
	}

	// Mobile chat controls collapse behind text buttons that open popovers
	//; null = no panel open. Desktop shows the controls inline.
	let mobilePanel = $state<'filters' | 'format' | 'auto' | null>(null);
	// The agent-side worker is gone once archived, so re-dispatch a fresh session
	// seeded with this one's config rather than trying to revive it.
	function newFromScript() {
		onNewFromScript?.(session);
	}

	// Nested dialogs and the rename input take Escape for themselves; keep it
	// from reaching the panel's document-level close handler.
	function guardEscape(e: KeyboardEvent) {
		if (e.key !== 'Escape') return;
		const target = e.target as Element | null;
		const own = (e.currentTarget as Element).closest('[role="dialog"]');
		if (target instanceof HTMLInputElement || target?.closest('[role="dialog"]') !== own) {
			e.preventDefault();
		}
	}
</script>

<div class="drawer-host">
<ResizablePanel
	mode="overlay"
	side="right"
	open
	{onclose}
	label={m.settings_group_conversation()}
	width={view.paneWidth ?? DRAWER_DEFAULT_PX}
	minWidth={DRAWER_MIN_PX}
	maxWidth="100vw"
	widthKey="cctui_drawer_width"
	fullWidthBelow="959px"
	handlePlacement="top"
	resizeStep={32}
>
	{#snippet panel()}
		<!-- svelte-ignore a11y_no_static_element_interactions -->
		<div class="drawer" data-journey="conversation" onkeydown={guardEscape}>
			<!-- The whole drawer is a file drop area: dragging files over it
			     shows the tsumikit Dropzone overlay; on drop they're staged as composer
			     attachments. overlay mode wraps the content without hijacking clicks. -->
			<Dropzone
				overlay
				multiple
				label={m.composer_drop_files()}
				disabled={!supportsAttachments || archived || queued}
				onfiles={(f) => composer?.addFiles(f)}
				onactive={(a) => composer?.setDragActive(a)}
			>
				<DrawerHeader
				{session}
				{archived}
				{isCodexSession}
				{livenessClass}
				{showStatusBadge}
				{onclose}
				onrename={sa.rename}
				onsetmodel={sa.setModel}
				oncopylink={sa.copyLink}
				oncopymarkdown={sa.copyMarkdown}
				onexport={sa.export}
				onfork={fork.openDialog}
				onforkselect={forkable
					? () => (selectMode ? exitSelect() : (selectMode = true))
					: undefined}
				forkSelectActive={selectMode}
				oninterrupt={sa.interrupt}
				onarchive={sa.archive}
				onstoparchive={sa.stopAndArchive}
				onTogglePin={togglePin}
				onAccountClick={() => (acctModalOpen = true)}
				{allLabels}
				onCreateLabel={createLabel}
				onAttachLabel={attachLabel}
				onDetachLabel={detachLabel}
				onUpdateLabel={updateLabel}
				onDeleteLabel={deleteLabel}
			/>

			{#if queued}
				<QueuedBanner {session} {onclose} />
			{:else}
			<DrawerToolbar
				hitCount={hits.count}
				hitIndex={hits.index}
				onprevhit={hits.prev}
				onnexthit={hits.next}
				bind:view
				autoApprove={session.auto_approve}
				bind:mobilePanel
				ontoggleAuto={sa.toggleAutoApprove}
				ondiagnose={() => (diagnoseOpen = true)}
				onterminal={() => (terminalOpen = !terminalOpen)}
				{terminalOpen}
				{pins}
				{lines}
				onjumpseq={(seq) => void ensureSeqVisible(seq)}
				onunpin={(seq) => void pinActions.unpin(id, seq)}
			/>

			<TaskPanel sessionId={id} progress={stream.todoProgress} />

			{#if diagnoseOpen}
				<DiagnosePanel sessionId={id} {session} onclose={closeDiagnose} />
			{/if}

			{#if terminalOpen}
				<TerminalPane sessionId={id} onclose={() => (terminalOpen = false)} />
			{/if}

			{#if needsInput}
				<div class="attn-banner">{m.conversation_waiting_input()}</div>
			{/if}

			{#if stream.softLimit}
				<!-- Slim notice once the auto-opened modal is dismissed, so the stalled chat
				     keeps an obvious way back to the switcher. -->
				<div class="attn-banner soft-limit-notice">
					<span>{m.conversation_soft_limit_reached({ account: stream.softLimit.account_name })}</span>
					<Button size="sm" tone="warn" onclick={() => (acctModalOpen = true)}>
						{m.conversation_switch_account()}
					</Button>
				</div>
			{/if}

			{#if acctModalOpen}
				<AccountSwitchModal
					sessionId={id}
					accounts={accounts.data ?? []}
					softLimit={stream.softLimit}
					onswitch={(acct) => stream.switchAccount(acct)}
					onclose={() => (acctModalOpen = false)}
				/>
			{/if}

			<Conversation
				bind:this={conv}
				{stream}
				{scroll}
				sessionId={id}
				{lines}
				isLoading={history.isLoading}
				canFetchOlder={canFetchEarlier}
				fetchingOlder={fetchingEarlier}
				onfetcholder={fetchEarlier}
				{archived}
				{askPreambleHtml}
				{planPreambleHtml}
				onedit={editPending}
				onrespondperm={(rid, allow) => ws.respondPermission(id, rid, allow)}
				{forkable}
				{selectMode}
				{selected}
				ontoggleselect={toggleSelect}
				{pinnedSeqs}
				onpin={togglePinLine}
				bind:jumper={renderWindow}
				{focusTs}
				onbookmark={(ln) => (bookmarkDraft = draftFromLine(ln, id, session.name ?? null))}
				{isBookmarked}
			/>

			<ActivityBanner {stream} {archived} />

			<ConversationComposer
				bind:this={composer}
				{session}
				{archived}
				working={stream.working}
				{supportsAttachments}
				{scroll}
				onsend={(body) => stream.sendBody(body)}
				stageFiles={(files) => actions.stageFiles(id, files)}
				onNewFromScript={newFromScript}
				onFork={fork.openDialog}
				onResume={sa.resume}
			/>
			{/if}
			</Dropzone>
		</div>

		{#if selectMode}
			<div class="fork-select-bar row">
				{#if selected.size > 0}
					<span class="fork-select-count">{selected.size}</span>
				{/if}
				<Button variant="primary" onclick={forkSelection} disabled={selected.size === 0}>
					{m.fork_selection()}
				</Button>
				<Button onclick={fork.openDialog}>{m.drawer_fork_label()}</Button>
				<Button onclick={exitSelect}>{m.common_cancel()}</Button>
			</div>
		{/if}

		{#if fork.open}
			<ForkModal
				{archived}
				{isCodexSession}
				parentTokens={fork.parentTokens}
				models={fork.models}
				efforts={fork.efforts}
				forking={fork.forking}
				extractLabel={fork.extractLabel}
				bind:model={fork.model}
				bind:effort={fork.effort}
				oncancel={fork.cancel}
				onsubmit={fork.submit}
			/>
		{/if}
	{/snippet}
</ResizablePanel>

{#if bookmarkDraft}
	<BookmarkSaveModal
		heading={m.bookmarks_save_title()}
		saveLabel={m.bookmarks_save_action()}
		title={bookmarkDraft.title}
		onsave={saveBookmark}
		onclose={() => (bookmarkDraft = null)}
	/>
{/if}
</div>

<style>
	.fork-select-bar {
		position: fixed;
		bottom: 1rem;
		left: 50%;
		transform: translateX(-50%);
		z-index: 199;
		gap: var(--sp-2);
		align-items: center;
		flex-wrap: nowrap;
		padding: var(--sp-2) var(--sp-3);
		background: var(--bg-elevated-2);
		border: 1px solid var(--border-strong);
		border-radius: var(--r-md);
		box-shadow: var(--shadow-lg, 0 8px 24px rgba(0, 0, 0, 0.5));
		white-space: nowrap;
	}
	.fork-select-count {
		font-variant-numeric: tabular-nums;
		font-weight: 600;
		opacity: 0.85;
		padding-left: var(--sp-1);
	}
	/* Zero-size stacking-context host so the fixed panel and scrim paint on
	   the drawer layer instead of inside the page's own stacking order. */
	.drawer-host {
		position: relative;
		z-index: var(--z-drawer);
	}
	.drawer {
		display: flex;
		flex-direction: column;
		height: 100%;
		background: var(--bg);
		padding-top: var(--safe-top);
	}
	.attn-banner {
		padding: var(--sp-2) var(--sp-3);
		background: var(--attention-bg);
		border-bottom: 1px solid var(--attention-bar);
		color: var(--warn);
		font-size: var(--fs-sm);
		font-weight: var(--fw-medium);
	}
	.soft-limit-notice {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--sp-2);
	}
</style>
