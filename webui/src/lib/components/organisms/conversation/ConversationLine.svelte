<script lang="ts">
	// A single rendered conversation message (assistant/user/system/tool/result),
	// extracted from ConversationDrawer. Pure presentation: it renders the meta
	// row (role badge, tool name, time, delivery state), the bubble, and the
	// per-message action buttons, delegating retry/edit/save/copy to callbacks.
	import TokenUsage from '$lib/components/molecules/TokenUsage.svelte';
	import { Badge, Button, Icon, IconButton, Text, Timestamp, Tooltip } from '@dorsk/tsumikit';
	import TurnSummaryFooter from './TurnSummaryFooter.svelte';
	import UserAttachments from './UserAttachments.svelte';
	import type { Line } from './types';
	import { m } from '$lib/paraglide/messages';
	import { settings } from '$lib/settings.svelte';
	import './bubble.css';

	let {
		ln,
		archived,
		sessionId = null,
		onretry,
		onedit,
		onsaveimage,
		oncopymarkdown,
		forkable = false,
		selectMode = false,
		selectedForFork = false,
		ontoggleselect,
		pinned = false,
		onpin,
		onbookmark,
		bookmarked = false
	}: {
		ln: Line;
		archived: boolean;
		/** Fallback owner of this line's uploads: Claude's own copy of the turn
		 * carries the filenames but not the staged path the session id is scraped
		 * from, so without this the thumbnail can never resolve. */
		sessionId?: string | null;
		onretry: (ts: number) => void;
		onedit: (text: string, ts: number) => void;
		onsaveimage: (e: MouseEvent, ln: Line) => void;
		oncopymarkdown: (ln: Line) => void;
		// Subset-fork affordances: only assistant lines carry the
		// `messageId` anchor shared by the line and the on-disk transcript.
		forkable?: boolean;
		selectMode?: boolean;
		selectedForFork?: boolean;
		onforkfrom?: (messageId: string) => void;
		onforkafter?: (messageId: string) => void;
		ontoggleselect?: (messageId: string) => void;
		pinned?: boolean;
		/** Toggle the pin on this line; omit to hide the action. */
		onpin?: (ln: Line) => void;
		/** Save this message to the cross-session bookmarks collection (CCT-992);
		 * omit to hide the action. */
		onbookmark?: (ln: Line) => void;
		bookmarked?: boolean;
	} = $props();

	// An optimistic user echo carries a synthetic `maxSeq + 1` seq that no server
	// row backs, so it must not be pinnable until the send is confirmed.
	const pinnable = $derived(
		!!onpin && typeof ln.seq === 'number' && !ln.pending && !ln.failed
	);

	const uploadRefs = $derived(
		ln.uploads ? { ...ln.uploads, sessionId: ln.uploads.sessionId ?? sessionId } : null
	);

	const forkAnchor = $derived(
		forkable && ln.role === 'assistant' && ln.messageId ? ln.messageId : null
	);

	function durationLabel(ms: number | undefined): string {
		if (!ms || ms < 1000) return '';
		const secs = Math.round(ms / 1000);
		if (secs < 60) return `${secs}s`;
		const mins = Math.floor(secs / 60);
		return `${mins}m ${secs % 60}s`;
	}

	// Thinking runs long; clamp it and offer a toggle, but only once the content
	// actually overflows the clamp. Measuring while expanded would report no
	// overflow and take the "show less" control away, so skip it then.
	let thinkingEl = $state<HTMLElement>();
	let thinkingExpanded = $state(false);
	let thinkingOverflows = $state(false);
	$effect(() => {
		void ln.html;
		if (thinkingExpanded || !thinkingEl) return;
		thinkingOverflows = thinkingEl.scrollHeight > thinkingEl.clientHeight + 1;
	});
</script>

<div
	class="line {ln.role}"
	class:tinted={settings.roleTintedBackground}
	class:pinned
	data-seq={ln.seq}
	data-journey="line"
	data-journey-key={ln.role}
	class:mcp={ln.mcp}
	class:pending={ln.pending}
	class:failed={!!ln.failed}
>
	<div class="lmeta row">
		{#if selectMode && forkAnchor}
			<input
				type="checkbox"
				class="fork-select-check"
				checked={selectedForFork}
				aria-label={m.fork_select_message_aria()}
				title={m.fork_select_message_title()}
				onchange={() => ontoggleselect?.(forkAnchor)}
			/>
		{/if}
		{#if ln.role === 'assistant' && ln.turn !== undefined}
			<Tooltip text={`turn ${ln.turn}`}>
				{#snippet trigger()}<Badge size="xs" uppercase color="var(--bc)">{ln.role}</Badge>{/snippet}
			</Tooltip>
		{:else}
			<Badge size="xs" uppercase color="var(--bc)"
				>{ln.mcp ? 'mcp' : ln.role === 'result' ? 'result' : ln.role}</Badge
			>
		{/if}
		{#if ln.role === 'tool' || ln.role === 'result'}
			<span class="who tool-name">{ln.role === 'result' ? '↳ ' : ''}{ln.tool ?? 'tool'}</span>
		{/if}
		{#if ln.role === 'peer' && ln.peerFrom}
			<span class="who peer-from" title={ln.peerFrom}>· {ln.peerFrom}</span>
		{/if}
		{#if ln.role === 'marker'}
			<span class="marker-ts"><Timestamp value={ln.ts} mode="time" tone="faint" size="xs" /></span>
		{:else}
			<Timestamp value={ln.ts} mode="time" tone="faint" size="xs" />
		{/if}
		{#if ln.failed}
			<span class="meta-end">
				<Text tone="danger" size="xs" nowrap title={ln.failed}>{m.conversation_not_delivered()}</Text>
			</span>
			{#if !archived}
				<Button
					variant="link"
					tone="danger"
					title={m.conversation_resend_title({ reason: ln.failed })}
					onclick={() => onretry(ln.ts)}>↻ {m.common_retry()}</Button>
				<IconButton
					inline
					icon="edit"
					label={m.conversation_edit_message_label()}
					title={m.conversation_edit_message_title()}
					onclick={() => onedit(ln.text ?? '', ln.ts)}
				/>
			{/if}
		{:else if ln.pending}
			<span class="meta-end">
				{#if ln.retrying}
					<Text tone="warn" size="xs" title={m.conversation_retrying_title()}
						>{m.conversation_retrying({ attempt: ln.retrying.attempt, max: ln.retrying.max })}</Text
					>
				{:else}
					<Text tone="warn" size="xs">{m.conversation_sending()}</Text>
				{/if}
			</span>
			{#if !archived}
				<IconButton
					inline
					icon="edit"
					label={m.conversation_edit_pending_label()}
					title={m.conversation_edit_pending_title()}
					onclick={() => onedit(ln.text ?? '', ln.ts)}
				/>
			{/if}
		{/if}
		<span class="line-actions" class:has-pin={pinned} data-journey="line-actions">
			{#if pinnable}
				<button
					type="button"
					class="pin-btn"
					class:on={pinned}
					aria-pressed={pinned}
					aria-label={pinned ? m.conversation_unpin_label() : m.conversation_pin_label()}
					title={pinned ? m.conversation_unpin_title() : m.conversation_pin_title()}
					onclick={() => onpin?.(ln)}><Icon name="pin" size={16} filled={pinned} /></button
				>
			{/if}
			<!-- Copy-as-Markdown uses the same markdown glyph as the
			     conversation-level copy; save-as-image uses a
			     plain image icon and sits right next to it. -->
			<IconButton
				inline
				glyphSize={16}
				icon="image"
				label={m.conversation_save_image_label()}
				title={m.conversation_save_image_title()}
				onclick={(e) => onsaveimage(e, ln)}
			/>
			<IconButton
				inline
				glyphSize={16}
				icon="markdown"
				label={m.conversation_copy_markdown_label()}
				title={m.conversation_copy_markdown_title()}
				onclick={() => oncopymarkdown(ln)}
			/>
			{#if onbookmark}
				<button
					type="button"
					class="bookmark"
					class:saved={bookmarked}
					aria-label={m.bookmarks_line_label()}
					title={bookmarked ? m.bookmarks_line_saved_title() : m.bookmarks_line_title()}
					onclick={() => onbookmark?.(ln)}>◈</button
				>
			{/if}
		</span>
	</div>
	{#if ln.role === 'thinking'}
		<div
			class="bubble think"
			class:redacted={ln.redacted}
			class:clamped={!thinkingExpanded}
			bind:this={thinkingEl}
		>
			{@html ln.html}
		</div>
		{#if thinkingOverflows}
			<Button
				variant="link"
				size="sm"
				shrink={false}
				style="margin-top:2px;color:var(--role-thinking)"
				aria-expanded={thinkingExpanded}
				onclick={() => (thinkingExpanded = !thinkingExpanded)}
			>
				{thinkingExpanded ? m.conversation_show_less() : m.conversation_show_more()}
			</Button>
		{/if}
	{:else if ln.role === 'marker'}
		<div class="marker-body">
			{#each ln.markerTexts ?? [ln.text ?? ''] as mt, i (i)}
				<span class="marker-item">{mt}</span>
			{/each}
		</div>
	{:else if ln.html}
		<div class="bubble">{@html ln.html}</div>
	{:else if ln.htmlCode}
		<pre class="bubble mono code">{@html ln.htmlCode}</pre>
	{:else if ln.text}
		<pre class="bubble mono code">{ln.text}</pre>
	{/if}
	{#if ln.attachmentCount}
		<div class="line-foot row">
			<Text tone="faint" size="xs"
				>{m.conversation_attachment_count({ count: ln.attachmentCount })}</Text
			>
		</div>
	{/if}
	{#if uploadRefs && uploadRefs.names.length}
		<UserAttachments refs={uploadRefs} ts={ln.ts} {archived} />
	{/if}
	{#if ln.summary}
		<TurnSummaryFooter summary={ln.summary} />
	{/if}
	{#if (ln.durationMs || ln.usage) && (ln.role === 'assistant' || ln.role === 'result')}
		{@const dur = durationLabel(ln.durationMs)}
		<div class="line-foot row">
			<!-- How long the model took to reply (CCT) — kept alongside the per-reply
			     token breakdown (no Σ; that's the conversation-wide aggregate). -->
			{#if dur}<Text tone="faint" size="xs">⏱ {dur}</Text>{/if}
			{#if ln.usage}<TokenUsage usage={ln.usage} showSum={false} />{/if}
		</div>
	{/if}
	{#if ln.stopHook}
		<div class="line-foot row"><Text tone="faint" size="xs">⏹ {ln.stopHook}</Text></div>
	{/if}
	{#if ln.fileHistory?.length}
		<div class="line-foot row">
			<Text tone="faint" size="xs" title={ln.fileHistory.join('\n')}
				>✎ {ln.fileHistory.length}</Text
			>
		</div>
	{/if}
</div>

<style>
	.line {
		display: flex;
		flex-direction: column;
		gap: 2px;
		max-width: 100%;
		/* Role tint, inherited by the badge below. Set on the line (not on the
		   badge itself) so one :global rule serves every role. */
		--bc: var(--text-muted);
	}
	.line.user {
		--bc: var(--role-user);
	}
	.line.assistant {
		--bc: var(--role-assistant);
	}
	.line.thinking {
		--bc: var(--role-thinking);
	}
	.line.system {
		--bc: var(--role-system);
	}
	.line.peer {
		--bc: var(--role-peer);
	}
	.line.poll {
		--bc: var(--role-poll);
	}
	.line.marker {
		--bc: var(--text-faint);
	}
	.line.tool,
	.line.result {
		--bc: var(--role-tool);
	}
	.line.mcp {
		--bc: var(--role-mcp);
	}
	.lmeta {
		gap: var(--sp-2);
		font-size: var(--fs-xs);
		color: var(--text-faint);
	}
	.who {
		text-transform: uppercase;
		letter-spacing: 0.04em;
		font-weight: var(--fw-medium);
	}
	.peer-from {
		font-family: var(--font-mono);
		color: var(--role-peer);
		text-transform: none;
		letter-spacing: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		max-width: 40%;
	}
	.tool-name {
		font-family: var(--font-mono);
		color: var(--text-muted);
		text-transform: none;
		letter-spacing: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		max-width: 60%;
	}
	/* Role badge pill — rides on the tsumikit Badge atom (pill
	   shape, sizing); these overrides add the per-role tint via --role-* tokens
	   and the uppercase treatment Badge doesn't carry. */
	/* Per-message action buttons (copy-as-Markdown + save-image), pushed to
	   the right of the meta row. Excluded from the saved image. */
	.line-actions {
		margin-left: auto;
		display: inline-flex;
		align-items: center;
		gap: var(--sp-1);
	}
	/* The pin stays visible once set — it marks the line in the flow, so it
	   cannot be a hover-only affordance like the copy buttons. */
	.pin-btn {
		display: inline-flex;
		align-items: center;
		padding: 0 var(--sp-1);
		background: none;
		border: none;
		line-height: 1;
		color: var(--text-faint);
		cursor: pointer;
	}
	.pin-btn:hover,
	.pin-btn.on {
		color: var(--warn);
	}
	/* Pinned line marker: a warm rail down its left edge. */
	.line.pinned {
		border-left: 2px solid var(--warn);
		padding-left: var(--sp-2);
		margin-left: calc(-1 * var(--sp-2));
	}
	.line-actions .bookmark {
		display: inline-flex;
		align-items: center;
		padding: var(--sp-1);
		background: none;
		border: 0;
		line-height: 1;
		cursor: pointer;
		font-size: var(--fs-sm);
		color: var(--text-muted);
	}
	.line-actions .bookmark:hover {
		color: var(--text);
	}
	.line-actions .bookmark.saved {
		color: var(--role-assistant);
	}
	/* Layout only; typography (faint xs) is the Text atom's. */
	.line .line-foot {
		align-self: flex-end;
		padding-inline: var(--sp-1);
	}
	/* Opt-in (Settings › Sessions): the whole bubble background takes the
	   role colour, on top of the rails below. Mixed into --bg-elevated so it
	   follows light and dark themes alike; user/system go a step stronger
	   than their always-on tint so they still stand apart. */
	.line.tinted.assistant .bubble {
		background: color-mix(in srgb, var(--role-assistant) 11%, var(--bg-elevated));
	}
	.line.tinted.tool .bubble,
	.line.tinted.result .bubble {
		background: color-mix(in srgb, var(--role-tool) 11%, var(--bg-elevated));
	}
	.line.tinted.mcp .bubble {
		background: color-mix(in srgb, var(--role-mcp) 11%, var(--bg-elevated));
	}
	.line.tinted.user .bubble {
		background: color-mix(in srgb, var(--role-user) 22%, var(--bg-elevated));
	}
	.line.tinted.system .bubble {
		background: color-mix(in srgb, var(--role-system) 20%, var(--bg-elevated));
	}
	.line.tinted.thinking .bubble {
		background: color-mix(in srgb, var(--role-thinking) 16%, var(--bg-elevated));
	}
	/* Uniform role tints — all via --role-* tokens. */
	.line.user .bubble {
		background: color-mix(in srgb, var(--role-user) 14%, var(--bg-elevated));
		border-color: color-mix(in srgb, var(--role-user) 45%, transparent);
	}
	.line.assistant .bubble {
		border-left: 2px solid color-mix(in srgb, var(--role-assistant) 55%, transparent);
	}
	/* System/agent-directed messages (harness wake-ups, task notifications,
	   injected reminders) — purple, distinct from the green user bubbles so
	   they don't read as something the human typed. */
	.line.system .bubble {
		background: color-mix(in srgb, var(--role-system) 12%, var(--bg-elevated));
		border-color: color-mix(in srgb, var(--role-system) 40%, transparent);
	}
	.line.peer .bubble {
		background: color-mix(in srgb, var(--role-peer) 12%, var(--bg-elevated));
		border-color: color-mix(in srgb, var(--role-peer) 40%, transparent);
	}
	.line.poll .bubble {
		background: color-mix(in srgb, var(--role-poll) 12%, var(--bg-elevated));
		border-color: color-mix(in srgb, var(--role-poll) 40%, transparent);
	}
	/* Harness bookkeeping (permission-mode flips, worktree/title updates) —
	   deliberately the quietest bubble in the log. */
	.line.marker .bubble {
		background: none;
		border-color: var(--border);
		color: var(--text-faint);
		font-size: var(--fs-xs);
	}
	/* Markers are bookkeeping, not messages: one quiet line, no bubble, and the
	   timestamp only on hover so a burst of them cannot dominate the log. */
	.line.marker .marker-body {
		display: flex;
		flex-wrap: wrap;
		gap: var(--sp-2);
		color: var(--text-faint);
		font-size: var(--fs-xs);
		line-height: 1.4;
	}
	.line.marker .marker-item::before {
		content: '· ';
	}
	.line.marker .marker-ts {
		visibility: hidden;
	}
	.line.marker:hover .marker-ts,
	.line.marker:focus-within .marker-ts {
		visibility: visible;
	}
	/* Optimistic reply: muted/amber until the agent acknowledges, then it
	   settles into the regular green user tint above. */
	.line.user.pending .bubble {
		background: color-mix(in srgb, var(--warn) 10%, var(--bg-elevated));
		border-color: color-mix(in srgb, var(--warn) 35%, transparent);
		opacity: 0.85;
	}
	/* Pushes the send-status text (and the controls after it) to the right. */
	.lmeta .meta-end {
		margin-left: auto;
	}
	/* Failed send: the bubble goes red and a Retry control appears. */
	.line.user.failed .bubble {
		background: color-mix(in srgb, var(--danger) 12%, var(--bg-elevated));
		border-color: color-mix(in srgb, var(--danger) 50%, transparent);
	}
	.line.tool .bubble,
	.line.result .bubble {
		background: var(--bg-elevated-2);
		border-left: 2px solid color-mix(in srgb, var(--role-tool) 55%, transparent);
	}
	.line.tool.mcp .bubble {
		border-left-color: color-mix(in srgb, var(--role-mcp) 60%, transparent);
	}
	/* Reasoning — muted brown, visually behind the prose it produced. */
	.line.thinking .bubble.think {
		background: color-mix(in srgb, var(--role-thinking) 10%, var(--bg-elevated));
		border-color: color-mix(in srgb, var(--role-thinking) 35%, transparent);
		border-left: 2px solid color-mix(in srgb, var(--role-thinking) 60%, transparent);
		color: color-mix(in srgb, var(--role-thinking) 45%, var(--md-text));
	}
	.line.thinking .bubble.think.clamped {
		max-height: 12rem;
		overflow: hidden;
		/* Fade the cut edge so a clamped block reads as truncated, not as ended. */
		mask-image: linear-gradient(to bottom, #000 8rem, transparent);
	}
	/* Provider withheld the content; only the placeholder remains. */
	.line.thinking .bubble.think.redacted {
		font-style: italic;
		opacity: 0.7;
	}
	.code {
		white-space: pre-wrap;
		max-height: 22rem;
		overflow: auto;
		font-size: calc(var(--fs-sm) - 0.0625rem);
	}
</style>
