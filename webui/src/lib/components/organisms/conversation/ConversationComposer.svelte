<script lang="ts">
	import { onDestroy } from 'svelte';
	import { imageAttachments } from '$lib/imageAttachments.svelte';
	import ImageCompressionStatus from '$lib/components/molecules/ImageCompressionStatus.svelte';
	const images = imageAttachments();
	onDestroy(() => images.reset());
	import { errMessage } from '$lib/api';
	import type { SessionListItem } from '@bindings/SessionListItem';
	import AttachmentList from '$lib/components/molecules/AttachmentList.svelte';
	import SessionMention from '$lib/components/molecules/SessionMention.svelte';
	import {
		pendingScheduled,
		useScheduledActions,
		useScheduledMessages,
		useSessionAttachments,
		useSessions
	} from '$lib/queries';
	import {
		Button,
		FileButton,
		IconButton,
		Input,
		InputGroup,
		Modal,
		SplitButton,
		Text,
		Textarea
	} from '@dorsk/tsumikit';
	import type { MenuItem } from '@dorsk/tsumikit';
	import ScheduledMessages from './ScheduledMessages.svelte';
	import { customBounds, parseCustom, schedulePresets, toLocalInput } from './scheduleTimes';
	import { drafts, composerKey, history as msgHistory } from '$lib/drafts';
	import { HistoryNav } from '$lib/historyNav';
	import {
		attachFiles,
		nextPasteIndex,
		rewriteFileTokens,
		removeFileByName,
		fileCapError,
		makeClipboardFiles
	} from '$lib/attachments';
	import { attachmentDraftSync, dropMissingTokens } from '$lib/attachmentStore';
	import { compact } from '$lib/format';
	import { toasts } from '$lib/toast.svelte';
	import type { ScrollController } from './scroll.svelte';
	import { cacheTtlMs } from './cacheTtl';
	import { m } from '$lib/paraglide/messages';
	import { settings } from '$lib/settings.svelte';
	import { routeEnter, showColdOffer } from '$lib/followup';

	let {
		session,
		archived,
		working,
		supportsAttachments,
		scroll,
		onsend,
		stageFiles,
		onNewFromScript,
		onFork,
		onResume,
		onFollowup
	}: {
		session: SessionListItem;
		archived: boolean;
		working: boolean;
		supportsAttachments: boolean;
		scroll: ScrollController;
		// Send a final message body (text + any appended staged-attachment paths).
		onsend: (body: string) => void;
		// Upload staged attachments, returning their absolute paths.
		stageFiles: (files: File[]) => Promise<{ paths: string[] }>;
		onNewFromScript: () => void;
		onFork: () => void;
		onResume: () => void;
		onFollowup?: (instruction?: string) => void;
	} = $props();

	// `#` mention popover source: the shared (cached) session list.
	const sessionsQuery = useSessions(() => false);
	const mentionSessions = $derived(sessionsQuery.data?.sessions ?? []);

	// Composer draft, persisted per session in localStorage. Initialized once (the
	// drawer instance persists across session switches; matching the original we do
	// NOT reload input on switch — only the history-nav cursor resets, below).
	// svelte-ignore state_referenced_locally
	let input = $state(drafts.get(composerKey(session.id)));
	$effect(() => {
		drafts.set(composerKey(session.id), input);
	});

	// ── Mid-chat file attachments ────────────────────────────────
	// Persisted per session in IndexedDB next to the localStorage draft. On send
	// we upload first, then append the staged paths under the message text so
	// the agent reads them.
	let attachments = $state<File[]>([]);
	const draftKey = $derived(composerKey(session.id));
	const attachmentSync = attachmentDraftSync();
	// Key of the session whose attachments are loaded; null while a restore is
	// in flight so a session switch never writes the old list under the new key.
	let attachmentsKey = $state<string | null>(null);
	$effect(() => {
		if (!attachmentsKey) return;
		void attachmentSync.persist(attachmentsKey, [...attachments]);
	});
	$effect(() => {
		const key = draftKey;
		images.reset();
		attachmentsKey = null;
		let live = true;
		(async () => {
			const restored = await attachmentSync.restore(key);
			if (!live || !restored) return;
			attachments = restored.files;
			const { text, dropped } = dropMissingTokens(input, restored.missing);
			if (dropped) {
				input = text;
				toasts.info(m.attachments_missing_dropped({ count: dropped }));
			}
			attachmentsKey = key;
		})();
		return () => {
			live = false;
		};
	});
	let uploading = $state(false);
	let dragActive = $state(false);
	const attachError = $derived(fileCapError(attachments));
	export function addFiles(incoming: File[]) {
		if (!supportsAttachments || archived || uploading) return;
		images.add(incoming, (file) => {
			({ files: attachments, text: input } = attachFiles(attachments, input, [file]));
		}, (file) => toasts.error(m.attachments_compression_failed({ name: file.name })));
	}
	export function setDragActive(active: boolean) {
		dragActive = active;
	}
	const removeAttachment = (name: string) => (attachments = removeFileByName(attachments, name));

	// Mask a large pasted block: instead of dumping thousands of
	// characters into the composer, collapse it into a `paste-N.txt` attachment
	// (the Claude Code trick), keeping the textarea readable. The index is derived
	// from current attachments, draft tokens and the session's staged names: the
	// composer remounts on drawer close while the draft (and its `[paste-N.txt]`)
	// persists per session.
	const PASTE_MASK_CHARS = 2000;
	const clipboardBinaryFiles = makeClipboardFiles();
	// A new draft starts with no tokens of its own, so the names the session has
	// already staged are the only thing keeping the next paste off `paste-1.txt`.
	const stagedQuery = useSessionAttachments(
		() => session.id,
		() => supportsAttachments && !archived
	);
	const stagedNames = $derived((stagedQuery.data ?? []).map((a) => a.name));

	function onPaste(e: ClipboardEvent) {
		if (!supportsAttachments || archived) return;
		const cd = e.clipboardData;
		if (!cd) return;
		// Binary clipboard content (pasted screenshot/image or copied file) → attach
		// it via the same staged-upload path as the 📎 picker and drag-and-drop.
		const files = clipboardBinaryFiles(cd);
		if (files.length > 0) {
			e.preventDefault();
			addFiles(files);
			return;
		}
		const text = cd.getData('text/plain');
		if (!text || text.length < PASTE_MASK_CHARS) return; // small → normal paste
		e.preventDefault();
		const name = `paste-${nextPasteIndex(attachments, input, stagedNames)}.txt`;
		addFiles([new File([text], name, { type: 'text/plain' })]);
		const lines = text.split('\n').length;
		toasts.ok(m.composer_large_paste({ name, lines }));
	}

	// ── Cold-cache Send button ───────────────────────────────────
	// Once the prompt cache lapses the next send re-writes the whole context to
	// cache (an expensive "burst"). The button's "cold now" is purely time-based.
	// The TTL window is provider/family- and model-dependent: Anthropic
	// 60m, OpenAI GPT-5.6+ 30m, else the 5-min legacy sliding window.
	const CACHE_TTL_MS = $derived(cacheTtlMs(session.adapter_id, session.model ?? null));
	// Final-minute countdown window.
	const COLD_WARN_MS = 60 * 1000;
	let now = $state(Date.now());
	const lastActivityMs = $derived(
		session.last_activity_at ? new Date(session.last_activity_at).getTime() : null
	);
	// The cache window is anchored to the last FINISHED turn.
	// While a turn is in flight (`working`) suppress the cold/countdown UI so it
	// can't flip "cold" mid-turn; it re-anchors off the new reply once the turn ends.
	const cacheCold = $derived(
		!working && lastActivityMs !== null && now - lastActivityMs > CACHE_TTL_MS
	);
	const burstTokens = $derived(session.estimated_burst_tokens ?? null);
	const msUntilCold = $derived(
		lastActivityMs === null ? null : CACHE_TTL_MS - (now - lastActivityMs)
	);
	const coldImminent = $derived(
		!working && msUntilCold !== null && msUntilCold > 0 && msUntilCold <= COLD_WARN_MS
	);
	const coldCountdownSecs = $derived(coldImminent ? Math.ceil(msUntilCold! / 1000) : null);
	// Tick fast (1s) only while counting down; otherwise a lazy 15s tick is enough
	// to flip the button cold.
	$effect(() => {
		const fast = coldImminent;
		const t = setInterval(() => (now = Date.now()), fast ? 1_000 : 15_000);
		return () => clearInterval(t);
	});

	// ── Scheduled send ───────────────────────────────────────────
	const scheduled = useScheduledMessages(() => session.id);
	const scheduledActions = useScheduledActions(() => session.id);
	const scheduledCount = $derived(pendingScheduled(scheduled.data).length);
	let scheduledEl = $state<HTMLElement>();
	let customOpen = $state(false);
	let customValue = $state('');
	const canSchedule = $derived(
		!!input.trim() && attachments.length === 0 && !uploading && images.pending.length === 0
	);

	const hhmm = (d: Date) => d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
	const scheduleItems = $derived.by<MenuItem[]>(() => {
		const presets: MenuItem[] = schedulePresets(new Date(now)).map((p) => ({
			label:
				p.id === 'later'
					? m.composer_schedule_later_today({ time: hhmm(p.at) })
					: p.id === 'tomorrow'
						? m.composer_schedule_tomorrow({ time: hhmm(p.at) })
						: m.composer_schedule_monday({ time: hhmm(p.at) }),
			icon: 'clock',
			disabled: !canSchedule,
			onselect: () => void scheduleAt(p.at)
		}));
		return [
			...presets,
			{
				label: m.composer_schedule_custom(),
				disabled: !canSchedule,
				onselect: () => {
					customValue = toLocalInput(new Date(Date.now() + 3_600_000));
					customOpen = true;
				}
			},
			{
				label: m.composer_schedule_list({ count: String(scheduledCount) }),
				disabled: scheduledCount === 0,
				onselect: () => scheduledEl?.scrollIntoView({ block: 'nearest', behavior: 'smooth' })
			}
		];
	});

	async function scheduleAt(at: Date) {
		const text = input.trim();
		if (!text || archived || attachments.length) return;
		try {
			await scheduledActions.schedule(text, at);
		} catch (e) {
			toasts.error(m.composer_schedule_failed({ message: errMessage(e) }));
			return;
		}
		toasts.info(
			m.composer_schedule_toast({
				when: at.toLocaleString([], {
					weekday: 'short',
					hour: '2-digit',
					minute: '2-digit'
				})
			})
		);
		msgHistory.push(session.id, text);
		input = '';
		resetHistoryNav();
		drafts.clear(composerKey(session.id));
	}

	function scheduleCustom() {
		const at = parseCustom(customValue, new Date());
		if (!at) {
			toasts.error(m.composer_schedule_custom_invalid());
			return;
		}
		customOpen = false;
		void scheduleAt(at);
	}

	let coldOfferDismissed = $state<string | null>(null);
	const coldOffer = $derived(
		!!onFollowup &&
			showColdOffer(settings.followupWhenCold, cacheCold, coldOfferDismissed === session.id)
	);
	function followup() {
		onFollowup?.(input.trim() || undefined);
	}
	function submit() {
		if (onFollowup && routeEnter(settings.followupWhenCold, cacheCold, false) === 'followup') {
			followup();
			return;
		}
		send();
	}

	const nav = new HistoryNav({
		list: () => msgHistory.get(session.id),
		value: () => input,
		setValue: (v) => (input = v),
		el: () => scroll.textarea
	});
	function resetHistoryNav() {
		nav.reset();
	}
	$effect(() => {
		void session.id;
		nav.resetAll();
	});

	// Pull a still-pending message back into the composer to edit + resend.
	export function loadDraft(text: string) {
		input = text;
		resetHistoryNav();
		scroll.textarea?.focus();
	}

	async function send() {
		const text = input.trim();
		// Allow sending attachments with no text (the staged paths become the
		// message), but require at least one of text/attachments.
		if ((!text && attachments.length === 0) || archived || uploading || images.pending.length) return;
		if (attachError) {
			toasts.error(attachError);
			return;
		}
		// Stage any pending attachments first; append the staged absolute paths under
		// the message so the agent reads them. On failure keep the draft +
		// attachments intact and surface the error rather than sending a half-message.
		let body = text;
		if (attachments.length) {
			uploading = true;
			try {
				const { paths } = await stageFiles(attachments);
				const prose = rewriteFileTokens(text, attachments, paths);
				const list = paths.map((p) => `- ${p}`).join('\n');
				const header = paths.length === 1 ? 'Attached file:' : `Attached files (${paths.length}):`;
				body = prose ? `${prose}\n\n${header}\n${list}` : `${header}\n${list}`;
				attachments = [];
				attachmentsKey = draftKey;
				void attachmentSync.discard(draftKey);
			} catch (e) {
				toasts.error(m.composer_attachment_upload_failed({ message: errMessage(e) }));
				return;
			} finally {
				uploading = false;
			}
		}
		sendBody(body);
	}

	// Hand the final body off to the parent's send orchestration, then clear the
	// composer + record the sent message in history.
	function sendBody(text: string) {
		if (!text || archived) return;
		onsend(text);
		msgHistory.push(session.id, text);
		input = '';
		resetHistoryNav();
		drafts.clear(composerKey(session.id));
	}

	// On touch/mobile, a bare Enter should insert a newline (the on-screen
	// keyboard's return key is easy to hit by accident) — send only via the Send
	// button or Ctrl/Cmd+Enter. On desktop, Enter sends and Shift+Enter newlines.
	const coarsePointer =
		typeof window !== 'undefined' &&
		typeof window.matchMedia === 'function' &&
		window.matchMedia('(pointer: coarse)').matches;

	// Plain Enter is the Textarea's `submitOn`; only history nav and the
	// always-available mod+Enter chord are handled here.
	function onKey(e: KeyboardEvent) {
		if (nav.handleKey(e)) return;
		if (e.key === 'Enter' && !coarsePointer && (e.ctrlKey || e.metaKey)) {
			e.preventDefault();
			send();
			return;
		}
		if (
			e.key === 'Enter' &&
			e.shiftKey &&
			!coarsePointer &&
			onFollowup &&
			routeEnter(settings.followupWhenCold, cacheCold, false) === 'followup'
		) {
			e.preventDefault();
			send();
		}
	}
</script>

<div class="composer" data-journey="composer" class:dropping={dragActive}>
	{#if archived}
		<div class="archived-actions">
			<div class="hint"><Text tone="muted" size="sm">{m.composer_archived_readonly()}</Text></div>
			<span class="archived-actions-btns">
				<Button onclick={onNewFromScript}>{m.composer_new_from_script()}</Button>
				<Button onclick={onFork}>{m.composer_fork()}</Button>
				<Button variant="primary" onclick={onResume}>{m.composer_resume()}</Button>
			</span>
		</div>
	{:else}
		<!-- Failed sends surface inline on the message bubble itself (red +
		     Retry), so there's no separate composer banner. -->
		<div class="scheduled" bind:this={scheduledEl}><ScheduledMessages sessionId={session.id} {archived} /></div>
		<ImageCompressionStatus pending={images.pending} />
		{#if supportsAttachments && attachments.length}
			<div class="attachments">
				<AttachmentList files={attachments} onremove={removeAttachment} compact />
			</div>
		{/if}
		{#if coldOffer}
			<div class="cold-offer">
				<Text tone="muted" size="sm">
					{m.composer_followup_offer()}
					<Button variant="link" size="sm" onclick={followup}>{m.composer_followup_start()}</Button>
				</Text>
				<IconButton
					icon="x"
					variant="ghost"
					box="xs"
					label={m.composer_followup_dismiss()}
					onclick={() => (coldOfferDismissed = session.id)}
				/>
			</div>
		{/if}
		{#snippet attach()}
			<FileButton
				label={m.composer_attach_files()}
				multiple
				box="sm"
				iconOnly
				variant="ghost"
				onfiles={addFiles}
			/>
		{/snippet}
		<!-- The `#` session-mention panel opens as a dropup above the field
		     (the composer is pinned to the bottom of the drawer). -->
		<InputGroup leading={supportsAttachments ? attach : undefined}>
			<SessionMention
				bind:value={input}
				el={scroll.textarea}
				sessions={mentionSessions}
				excludeId={session.id}
				placement="up"
			>
				<Textarea
					rows={1}
					autoresize
					resize="top"
					maxHeight="40vh"
					submitOn={coarsePointer ? 'mod-enter' : 'enter'}
					onsubmit={submit}
					data-journey="message"
					aria-label={m.a11y_composer_message()}
					placeholder={dragActive
						? m.composer_drop_files()
						: coarsePointer
							? m.composer_placeholder_message()
							: m.composer_placeholder_message_enter()}
					bind:value={input}
					bind:el={scroll.textarea}
					onkeydown={onKey}
					oninput={() => resetHistoryNav()}
					onpaste={onPaste}
				/>
			</SessionMention>
			{#snippet trailing()}
				<!-- Stays a plain primary button across all cost states: a `tone` on
				     `primary` recolors the label over the accent fill (unreadable). The
				     cold/imminent state lives in the label and the title tooltip. -->
				<SplitButton
					variant="primary"
					size="sm"
					label={m.composer_schedule_menu()}
					items={scheduleItems}
					placement="top-end"
					disabled={uploading || images.pending.length > 0 || (!input.trim() && attachments.length === 0)}
					onclick={send}
					title={cacheCold
						? burstTokens
							? m.composer_cache_cold_burst({ tokens: compact(burstTokens) })
							: m.composer_cache_cold()
						: coldImminent
							? m.composer_cache_imminent()
							: undefined}
				>
					{#if uploading}{m.composer_uploading()}{:else if coldImminent}{m.composer_send()} (<span
							class="countdown">{coldCountdownSecs}s</span
						>){:else if cacheCold && burstTokens}{m.composer_send()} ❄️ ~{compact(
							burstTokens
						)}{:else if cacheCold}{m.composer_send()}
						❄️{:else}{m.composer_send()}{/if}
				</SplitButton>
			{/snippet}
		</InputGroup>
	{/if}
</div>

{#if customOpen}
	{@const bounds = customBounds(new Date(now))}
	<Modal title={m.composer_schedule_custom_title()} onclose={() => (customOpen = false)} size="sm">
		{#snippet body()}
			<label class="custom-at">
				<Text size="sm">{m.composer_schedule_custom_label()}</Text>
				<Input
					type="datetime-local"
					min={bounds.min}
					max={bounds.max}
					bind:value={customValue}
					onenter={scheduleCustom}
				/>
			</label>
		{/snippet}
		{#snippet footer()}
			<Button onclick={() => (customOpen = false)}>{m.common_cancel()}</Button>
			<Button variant="primary" onclick={scheduleCustom}>{m.composer_schedule_confirm()}</Button>
		{/snippet}
	</Modal>
{/if}

<style>
	.composer {
		display: flex;
		flex-direction: column;
		padding-bottom: var(--safe-bottom);
		border-top: 1px solid var(--border);
		background: var(--bg-elevated);
	}
	/* The field runs edge to edge; the rows above it keep their own inset. */
	.scheduled,
	.cold-offer,
	.attachments,
	.archived-actions {
		padding: var(--sp-2) var(--sp-3);
	}
	.scheduled:empty {
		display: none;
	}
	/* Highlight the composer while a file drag hovers the conversation pane
	  . */
	.composer.dropping {
		outline: 2px dashed var(--c-blue);
		outline-offset: -2px;
		background: color-mix(in srgb, var(--c-blue) 8%, var(--bg-elevated));
	}
	.cold-offer {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--sp-2);
	}
	.attachments {
		width: 100%;
	}
	.archived-actions {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--sp-2);
	}
	/* Right-aligned action cluster: New from same script · Fork · Resume. */
	.archived-actions-btns {
		display: flex;
		align-items: center;
		gap: var(--sp-2);
		margin-left: auto;
	}
	/* Fixed-width, tabular digits so "59s"→"0s" doesn't jitter the button. The
	   countdown <span> is in this component's markup, so a scoped rule reaches it. */
	.countdown {
		display: inline-block;
		min-width: 2.4ch;
		text-align: right;
		font-variant-numeric: tabular-nums;
	}
	.custom-at {
		display: flex;
		flex-direction: column;
		gap: var(--sp-1);
	}
	.hint {
		text-align: center;
		width: 100%;
	}
</style>
