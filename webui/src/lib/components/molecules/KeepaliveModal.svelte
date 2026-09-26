<script lang="ts">
	import type { SessionListItem } from '@bindings/SessionListItem';
	import { Button, Field, Modal, Select, Switch, Text } from '@dorsk/tsumikit';
	import { useSessionActions } from '$lib/queries';
	import {
		DEFAULT_MAX_TICKS,
		defaultIntervalSecs,
		formatIntervalSecs,
		intervalOptionsSecs,
		MAX_TICKS_OPTIONS
	} from '$lib/components/organisms/conversation/keepalive';
	import { m } from '$lib/paraglide/messages';

	let { session, onclose }: { session: SessionListItem; onclose: () => void } = $props();

	const actions = useSessionActions();
	const current = $derived(session.keepalive ?? null);

	let enabled = $state(!!session.keepalive);
	let intervalSecs = $state(
		String(
			session.keepalive?.interval_secs ??
				defaultIntervalSecs(session.adapter_id, session.model ?? null)
		)
	);
	let maxTicks = $state(String(session.keepalive?.max_ticks ?? DEFAULT_MAX_TICKS));
	let saving = $state(false);
	let error = $state<string | null>(null);

	const options = $derived(intervalOptionsSecs(session.adapter_id, session.model ?? null));

	function ticksLabel(n: number): string {
		return n === 0 ? m.keepalive_ticks_indefinite() : m.keepalive_ticks_n({ n });
	}

	async function save() {
		saving = true;
		error = null;
		try {
			await actions.setKeepalive(session.id, {
				enabled,
				interval_secs: Number(intervalSecs),
				max_ticks: Number(maxTicks)
			});
			onclose();
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			saving = false;
		}
	}
</script>

<Modal title={m.keepalive_title()} busy={saving} {onclose}>
	{#snippet body()}
		<div class="ka-body">
			<Text as="p" tone="muted" size="sm">{m.keepalive_help()}</Text>
			<Switch bind:checked={enabled} label={m.keepalive_enable()} disabled={saving} />
			<Field label={m.keepalive_interval()}>
				<Select bind:value={intervalSecs} disabled={!enabled || saving}>
					{#each options as secs (secs)}
						<option value={String(secs)}>{formatIntervalSecs(secs)}</option>
					{/each}
				</Select>
			</Field>
			<Field label={m.keepalive_max_ticks()}>
				<Select bind:value={maxTicks} disabled={!enabled || saving}>
					{#each MAX_TICKS_OPTIONS as n (n)}
						<option value={String(n)}>{ticksLabel(n)}</option>
					{/each}
				</Select>
			</Field>
			{#if current}
				<Text size="xs" tone="faint">
					{m.keepalive_progress({ sent: current.ticks_sent, max: ticksLabel(current.max_ticks) })}
				</Text>
			{/if}
			{#if error}
				<div role="alert"><Text size="sm" tone="danger">{error}</Text></div>
			{/if}
		</div>
	{/snippet}
	{#snippet footer()}
		<Button size="sm" variant="ghost" onclick={onclose} disabled={saving}>{m.common_cancel()}</Button>
		<Button size="sm" variant="default" onclick={save} loading={saving} disabled={saving}>
			{m.common_save()}
		</Button>
	{/snippet}
</Modal>

<style>
	.ka-body {
		display: flex;
		flex-direction: column;
		gap: var(--sp-3);
		min-width: min(22rem, 90vw);
	}
</style>
