<script lang="ts">
	import { Button, Cluster, Input, Text, Textarea } from '@dorsk/tsumikit';
	import { errMessage } from '$lib/api';
	import {
		pendingScheduled,
		useScheduledActions,
		useScheduledMessages,
		type ScheduledMessage
	} from '$lib/queries';
	import { toasts } from '$lib/toast.svelte';
	import { m } from '$lib/paraglide/messages';
	import { customBounds, parseCustom, toLocalInput } from './scheduleTimes';

	let { sessionId, archived }: { sessionId: string; archived: boolean } = $props();

	const query = useScheduledMessages(() => sessionId);
	const actions = useScheduledActions(() => sessionId);
	const pending = $derived(pendingScheduled(query.data));

	let editing = $state<string | null>(null);
	let draftBody = $state('');
	let draftAt = $state('');
	let busy = $state<string | null>(null);

	function formatWhen(iso: string): string {
		return new Date(iso).toLocaleString([], {
			weekday: 'short',
			day: 'numeric',
			month: 'short',
			hour: '2-digit',
			minute: '2-digit'
		});
	}

	function startEdit(row: ScheduledMessage) {
		editing = row.id;
		draftBody = row.body;
		draftAt = toLocalInput(new Date(row.deliver_at));
	}

	async function act(id: string, fn: () => Promise<unknown>) {
		busy = id;
		try {
			await fn();
		} catch (e) {
			toasts.error(m.scheduled_action_failed({ message: errMessage(e) }));
		} finally {
			busy = null;
		}
	}

	function save(row: ScheduledMessage) {
		const at = parseCustom(draftAt, new Date());
		if (!at || !draftBody.trim()) {
			toasts.error(m.composer_schedule_custom_invalid());
			return;
		}
		void act(row.id, async () => {
			await actions.update(row.id, { body: draftBody, deliver_at: at.toISOString() });
			editing = null;
		});
	}
</script>

{#if pending.length}
	<section class="scheduled" aria-label={m.scheduled_panel_title()} data-journey="scheduled-messages">
		{#each pending as row (row.id)}
			<div class="pending" class:dead={row.state === 'dead'}>
				{#if editing === row.id}
					<Textarea rows={2} autoresize maxHeight="30vh" bind:value={draftBody} />
					<Cluster>
						<Input
							type="datetime-local"
							min={customBounds(new Date()).min}
							max={customBounds(new Date()).max}
							bind:value={draftAt}
						/>
						<Button size="sm" onclick={() => (editing = null)}>{m.common_cancel()}</Button>
						<Button
							size="sm"
							variant="primary"
							disabled={busy === row.id}
							onclick={() => save(row)}>{m.scheduled_save()}</Button
						>
					</Cluster>
				{:else}
					<div class="body">{row.body}</div>
					<div class="meta">
						<Text tone="faint" size="xs">
							{#if row.state === 'dead'}
								{m.scheduled_dead({ reason: row.last_error ?? '' })}
							{:else if row.attempts > 0}
								{m.scheduled_retrying({
									attempts: String(row.attempts),
									reason: row.last_error ?? ''
								})}
							{:else}
								{m.scheduled_pending_label({ when: formatWhen(row.deliver_at) })}
							{/if}
						</Text>
						{#if !archived}
							<Cluster>
								{#if row.state === 'scheduled'}
									<Button size="sm" variant="ghost" onclick={() => startEdit(row)}
										>{m.scheduled_edit()}</Button
									>
									<Button
										size="sm"
										variant="ghost"
										disabled={busy === row.id}
										onclick={() => act(row.id, () => actions.sendNow(row.id))}
										>{m.scheduled_send_now()}</Button
									>
								{/if}
								{#if row.state !== 'sending'}
									<Button
										size="sm"
										variant="ghost"
										disabled={busy === row.id}
										onclick={() => act(row.id, () => actions.cancel(row.id))}
										>{m.scheduled_cancel()}</Button
									>
								{/if}
							</Cluster>
						{/if}
					</div>
				{/if}
			</div>
		{/each}
	</section>
{/if}

<style>
	.scheduled {
		display: flex;
		flex-direction: column;
		align-items: flex-end;
		gap: var(--sp-2);
		max-height: 30vh;
		overflow-y: auto;
	}
	.pending {
		display: flex;
		flex-direction: column;
		gap: var(--sp-1);
		max-width: min(100%, 40rem);
		padding: var(--sp-2) var(--sp-3);
		border: 1px dashed color-mix(in srgb, var(--role-user) 50%, transparent);
		border-radius: var(--radius-md, 8px);
		background: color-mix(in srgb, var(--role-user) 6%, var(--bg-elevated));
		opacity: 0.7;
	}
	.pending.dead {
		border-color: color-mix(in srgb, var(--c-red) 60%, transparent);
	}
	.body {
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}
	.meta {
		display: flex;
		align-items: center;
		justify-content: space-between;
		flex-wrap: wrap;
		gap: var(--sp-2);
	}
</style>
