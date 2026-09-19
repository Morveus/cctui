<script lang="ts">
	// Stands in for the conversation and the composer while a session is
	// `queued`: its machine was over the RAM ceiling set for it, so the spawn
	// waits for the reaper to launch it. Says why, with the figures, and offers
	// the two human overrides: launch now past the ceiling, or cancel.
	import { Button, Callout, Cluster, ConfirmModal, Stack, Text, Timestamp } from '@dorsk/tsumikit';
	import type { SessionListItem } from '@bindings/SessionListItem';
	import { ApiError, errMessage } from '$lib/api';
	import { useSessionActions } from '$lib/queries';
	import { m } from '$lib/paraglide/messages';
	import { toasts } from '$lib/toast.svelte';
	import { launchUncertain, queuedFigures, queuedSummary } from '$lib/memCeiling';

	let { session, onclose }: { session: SessionListItem; onclose: () => void } = $props();

	const actions = useSessionActions();
	const figures = $derived(queuedFigures(session));
	// An interrupted launch: nobody can say whether the machine got it, so the
	// request is kept and only a human decides what happens next.
	const doubt = $derived(launchUncertain(session));
	let confirming = $state<'launch' | 'discard' | null>(null);
	let busy = $state(false);

	async function launchNow() {
		busy = true;
		try {
			await actions.launchDraft(session.id, {});
			toasts.ok(m.queued_toast_launched());
			// claude-code registers the live session under this same id, so the
			// page turns into it; other adapters mint their own id.
			if (session.adapter_id !== 'claude-code') onclose();
		} catch (e) {
			// A 409 on a session in doubt is the server refusing to guess, not
			// "already launching": show what it said rather than a wrong reason.
			toasts.error(
				e instanceof ApiError && e.status === 409 && !doubt
					? m.queued_toast_launch_conflict()
					: m.queued_toast_launch_failed({ error: errMessage(e) })
			);
		} finally {
			busy = false;
			confirming = null;
		}
	}

	async function discard() {
		busy = true;
		try {
			await actions.discardDraft(session.id);
			toasts.ok(doubt ? m.queued_toast_dropped() : m.queued_toast_discarded());
			onclose();
		} catch (e) {
			toasts.error(m.queued_toast_discard_failed({ error: errMessage(e) }));
		} finally {
			busy = false;
			confirming = null;
		}
	}
</script>

<div class="queued" data-journey="queued-banner">
	<Callout
		tone={doubt ? 'danger' : 'warn'}
		icon={doubt ? 'alert' : 'clock'}
		title={doubt ? m.queued_uncertain_title() : m.queued_banner_title()}
	>
		<Stack gap="var(--sp-2)">
			<Text size="sm">{doubt ? m.queued_uncertain_body({ why: doubt.why }) : m.queued_banner_body()}</Text>
			{#if doubt && doubt.since}
				<Text size="xs" tone="faint"
					>{m.queued_uncertain_since()}
					<Timestamp value={doubt.since} mode="relative" tone="faint" size="xs" /></Text
				>
			{/if}
			{#if figures && !doubt}
				<Text size="sm" weight="semibold">{queuedSummary(figures)}</Text>
				{#if figures.checked_at}
					<Text size="xs" tone="faint"
						>{m.queued_banner_checked()}
						<Timestamp value={figures.checked_at} mode="relative" tone="faint" size="xs" /></Text
					>
				{/if}
			{/if}
		</Stack>
		{#snippet actions()}
			<Cluster gap="var(--sp-2)">
				<Button size="sm" variant="primary" disabled={busy} onclick={() => (confirming = 'launch')}>
					{doubt ? m.queued_launch_anyway() : m.queued_launch_now()}
				</Button>
				<Button size="sm" disabled={busy} onclick={() => (confirming = 'discard')}>
					{doubt ? m.queued_drop() : m.queued_cancel()}
				</Button>
			</Cluster>
		{/snippet}
	</Callout>
</div>

{#if confirming === 'launch'}
	<ConfirmModal
		open
		tone="warn"
		title={doubt ? m.queued_confirm_relaunch_title() : m.queued_confirm_launch_title()}
		message={doubt ? m.queued_confirm_relaunch_body() : m.queued_confirm_launch_body()}
		confirmLabel={doubt ? m.queued_launch_anyway() : m.queued_launch_now()}
		{busy}
		onconfirm={launchNow}
		oncancel={() => (confirming = null)}
	/>
{:else if confirming === 'discard'}
	<ConfirmModal
		open
		tone="danger"
		title={doubt ? m.queued_confirm_drop_title() : m.queued_confirm_discard_title()}
		message={doubt ? m.queued_confirm_drop_body() : m.queued_confirm_discard_body()}
		confirmLabel={doubt ? m.queued_drop() : m.queued_cancel()}
		cancelLabel={m.queued_keep_waiting()}
		{busy}
		onconfirm={discard}
		oncancel={() => (confirming = null)}
	/>
{/if}

<style>
	.queued {
		flex: 1 1 auto;
		padding: var(--sp-3);
		overflow: auto;
	}
</style>
