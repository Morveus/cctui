<script lang="ts">
	import type { SessionListItem } from '@bindings/SessionListItem';
	import { CopyButton, Timestamp, Tooltip } from '@dorsk/tsumikit';
	import { m } from '$lib/paraglide/messages';
	import { sessionDebugRows } from '../../../routes/sessions/sessions.logic';
	import { sessionEnd } from '$lib/sessionEnd';
	import SessionDotFacts from './SessionDotFacts.svelte';

	// Activity dot: the liveness dot carries a rich debug tooltip —
	// session id (surfaced nowhere else, click-to-copy) plus the Process /
	// Transport / Account blocks and the session's own debug rows.
	// `livenessClass` and `now` are derived by the caller (SessionCard /
	// DrawerHeader) so the dot color and the stale/relative-age words stay in
	// sync with the row.
	let {
		session,
		livenessClass,
		now = Date.now()
	}: { session: SessionListItem; livenessClass: string; now?: number } = $props();

	// The daemon report costs a server → daemon round trip, so the facts child —
	// and its query — mounts only once the tooltip is armed.
	let armed = $state(false);

	const rows = $derived.by((): { label: string; value: string; at?: string }[] => {
		const base = sessionDebugRows(session, now);
		const end = sessionEnd(session);
		if (!end) return base;
		return [
			...base,
			{ label: m.sessions_dot_ended(), value: end.endedAt ? '' : '—', at: end.endedAt ?? undefined }
		];
	});
</script>

<Tooltip maxWidth="26rem">
	{#snippet trigger()}
		<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
		<span
			class="dot {livenessClass}"
			role="img"
			tabindex="0"
			aria-label={m.sessions_dot_aria()}
			onmouseenter={() => (armed = true)}
			onfocus={() => (armed = true)}
		></span>
	{/snippet}
	{#snippet content()}
		<div class="dbg">
			<div class="idrow">
				<code class="id">{session.id}</code>
				<CopyButton
					text={session.id}
					variant="ghost"
					box="xs"
					label={m.sessions_copy_id_title()}
				/>
			</div>
			{#if armed}
				<div class="blocks"><SessionDotFacts {session} {now} /></div>
			{/if}
			<dl class="grid">
				{#each rows as r (r.label)}
					<dt>{r.label}</dt>
					<dd>{#if r.at}<Timestamp value={r.at} size="xs" tone="inherit" />{:else}{r.value}{/if}</dd>
				{/each}
			</dl>
		</div>
	{/snippet}
</Tooltip>

<style>
	.dbg {
		font-size: var(--fs-xs);
		line-height: 1.4;
	}
	.idrow {
		display: flex;
		align-items: center;
		gap: var(--sp-2);
		margin-bottom: var(--sp-2);
	}
	.id {
		font-family: var(--font-mono, monospace);
		user-select: all;
		word-break: break-all;
		color: var(--text);
	}
	.blocks {
		margin-bottom: var(--sp-2);
	}
	.grid {
		display: grid;
		grid-template-columns: auto 1fr;
		column-gap: var(--sp-2);
		row-gap: 0.15rem;
		margin: 0;
	}
	.grid dt {
		color: var(--text-faint);
		font-family: var(--font-mono, monospace);
	}
	.grid dd {
		margin: 0;
		font-family: var(--font-mono, monospace);
		word-break: break-word;
		color: var(--text);
	}
</style>
