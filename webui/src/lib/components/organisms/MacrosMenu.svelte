<script lang="ts">
	// The Macros menu of the Sessions toolbar (Settings › Macros turns it on):
	// one row per macro; picking one spawns a session with the macro's knobs
	// and its prompt, after a confirmation when the macro asks for one. The
	// server archives the session on its own once the turn ends cleanly
	// (`auto_archive` on the spawn request).
	import { goto } from '$app/navigation';
	import { Button, Cluster, Menu, Modal, Text } from '@dorsk/tsumikit';
	import type { MenuItem } from '@dorsk/tsumikit';
	import { useSessionActions } from '$lib/queries';
	import { settings, type MacroSpec } from '$lib/settings.svelte';
	import { toasts } from '$lib/toast.svelte';
	import { ws } from '$lib/ws.svelte';
	import { m } from '$lib/paraglide/messages';
	import { isQueuedSpawn, notifyQueuedSpawn } from '$lib/memCeiling';
	import { macroProblems, spawnBodyFor } from './macros.logic';

	const actions = useSessionActions();
	let pending = $state<MacroSpec | null>(null);
	let running = $state<string | null>(null);

	const items = $derived<MenuItem[]>(
		settings.macros.length
			? settings.macros.map((mac) => ({
					label: mac.title,
					disabled: running === mac.id || macroProblems(mac).length > 0,
					tag: mac.confirm ? undefined : m.macros_menu_instant_tag(),
					onselect: () => pick(mac)
				}))
			: [{ label: m.macros_menu_empty(), disabled: true, onselect: () => {} }]
	);

	function pick(mac: MacroSpec) {
		if (mac.confirm) pending = mac;
		else void run(mac);
	}

	async function run(mac: MacroSpec) {
		pending = null;
		running = mac.id;
		try {
			const res = await actions.spawn(spawnBodyFor(mac), []);
			if (isQueuedSpawn(res)) {
				// Held back by the machine's RAM ceiling: nothing to wait for.
				notifyQueuedSpawn(res);
				if (res.session_id) void goto(`/sessions/${res.session_id}`);
				return;
			}
			toasts.info(m.macros_toast_started({ title: mac.title }));
			const result = await ws.awaitSpawn(res.command_id, res.session_id);
			if (result.ok && res.session_id) {
				void goto(`/sessions/${res.session_id}`);
			} else if (!result.ok && !result.timedOut) {
				toasts.error(m.spawn_toast_spawn_failed({ error: result.error ?? m.spawn_error_unknown() }));
			}
		} catch (e) {
			toasts.error(m.spawn_toast_spawn_failed({ error: e instanceof Error ? e.message : String(e) }));
		} finally {
			running = null;
		}
	}
</script>

<Menu label={m.macros_menu_label()} {items} placement="bottom-end">
	{#snippet trigger()}
		<span class="trig">⚡<span class="trig-label"> {m.macros_menu_label()}</span></span>
	{/snippet}
</Menu>

{#if pending}
	{@const mac = pending}
	<Modal title={m.macros_confirm_title({ title: mac.title })} onclose={() => (pending = null)} footerFill>
		{#snippet body()}
			<Text>{m.macros_confirm_body()}</Text>
			<pre class="prompt">{mac.prompt}</pre>
		{/snippet}
		{#snippet footer()}
			<Cluster>
				<Button grow onclick={() => (pending = null)}>{m.common_cancel()}</Button>
				<Button grow variant="primary" onclick={() => void run(mac)}>{m.macros_confirm_run()}</Button>
			</Cluster>
		{/snippet}
	</Modal>
{/if}

<style>
	.trig {
		white-space: nowrap;
	}
	.prompt {
		margin: var(--sp-2) 0 0;
		padding: var(--sp-2);
		max-height: 14rem;
		overflow: auto;
		white-space: pre-wrap;
		font-size: var(--fs-sm, 0.85rem);
		background: var(--bg-elevated);
		border-radius: var(--radius-sm, 4px);
	}
	@media (max-width: 640px) {
		.trig-label {
			display: none;
		}
	}
</style>
