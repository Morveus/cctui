<script lang="ts">
	// "A newer cctui is out" — opened from the red ↑ chip in the header.
	//
	// Step 1 shows the release notes the server's probe collected (no GitHub
	// call from the browser) and, for admins, the red "Update" button; other
	// users read "ask your administrator". Step 2 is the plain-language
	// consent, and what it promises depends on how this deployment updates:
	//
	//   - with an update hook, the machine runs the operator's own command,
	//     verifies the served version and rolls back on failure — step 3 then
	//     follows that run right here;
	//   - without one, a YOLO agent takes over and we jump to its session,
	//     because a transcript is the only progress an agent has.
	import { goto } from '$app/navigation';
	import { Button, Modal, Text } from '@dorsk/tsumikit';
	import { createQuery } from '@tanstack/svelte-query';
	import type { UpdateHookPhase } from '@bindings/UpdateHookPhase';
	import { endpoints, useMe } from '$lib/queries';
	import { renderMarkdown } from '$lib/markdown';
	import { toasts } from '$lib/toast.svelte';
	import { ws } from '$lib/ws.svelte';
	import { m } from '$lib/paraglide/messages';

	let {
		latestVersion,
		latestUrl,
		selfUpdateReady,
		selfUpdateHook,
		onclose
	}: {
		latestVersion: string;
		latestUrl: string;
		/** `VersionInfo.self_update_ready`: a self-update machine is configured. */
		selfUpdateReady: boolean;
		/** `VersionInfo.self_update_hook`: that machine updates without an agent. */
		selfUpdateHook: boolean;
		onclose: () => void;
	} = $props();

	const me = useMe();
	const isAdmin = $derived(me.data?.role === 'admin');
	const changelog = createQuery(() => ({
		queryKey: ['version', 'changelog', latestVersion],
		queryFn: endpoints.changelog,
		staleTime: 60_000
	}));

	let step = $state<'notes' | 'confirm' | 'run'>('notes');
	let launching = $state(false);

	// Only polls once a hook run is actually in flight, and stops itself the
	// moment the run reports a terminal phase.
	const run = createQuery(() => ({
		queryKey: ['version', 'self-update-run'],
		queryFn: endpoints.selfUpdateStatus,
		enabled: step === 'run',
		refetchInterval: (query) => (query.state.data?.done ? false : 3_000)
	}));

	const PHASE_LABEL: Record<UpdateHookPhase, () => string> = {
		running: m.update_run_phase_running,
		verifying: m.update_run_phase_verifying,
		succeeded: m.update_run_phase_succeeded,
		rolling_back: m.update_run_phase_rolling_back,
		rolled_back: m.update_run_phase_rolled_back,
		failed: m.update_run_phase_failed
	};

	const phase = $derived(run.data?.phase);
	const phaseTone = $derived(
		phase === 'succeeded' ? 'success' : phase === 'running' || phase === 'verifying' ? 'faint' : 'danger'
	);

	async function launch() {
		launching = true;
		try {
			const res = await endpoints.selfUpdate();
			if (res.mode === 'hook') {
				toasts.info(m.update_toast_launched_hook({ version: res.version }));
				step = 'run';
				return;
			}
			toasts.info(m.update_toast_launched({ version: res.version }));
			const ack = await ws.awaitCommand(String(res.command_id));
			if (ack.ok || ack.timedOut) {
				onclose();
				await goto(res.session_id ? `/sessions/${encodeURIComponent(res.session_id)}` : '/sessions');
			} else {
				toasts.error(m.update_toast_failed({ error: ack.error ?? m.spawn_error_unknown() }));
			}
		} catch (e) {
			toasts.error(m.update_toast_failed({ error: e instanceof Error ? e.message : String(e) }));
		} finally {
			launching = false;
		}
	}
</script>

{#if step === 'notes'}
	<Modal title={m.update_modal_title({ version: latestVersion })} {onclose} size="lg" {body} {footer} />
{:else if step === 'confirm'}
	<Modal title={m.update_confirm_title()} {onclose} size="sm" body={confirmBody} footer={confirmFooter} />
{:else}
	<Modal
		title={m.update_run_title({ version: latestVersion })}
		{onclose}
		size="md"
		body={runBody}
		footer={runFooter}
	/>
{/if}

{#snippet body()}
	<div class="notes">
		{#if changelog.isPending}
			<Text tone="faint" size="sm">{m.update_changelog_loading()}</Text>
		{:else if changelog.isError}
			<Text tone="danger" size="sm">{m.update_changelog_failed()}</Text>
		{:else if !changelog.data?.releases.length}
			<Text tone="faint" size="sm">{m.update_changelog_empty()}</Text>
		{:else}
			{#each changelog.data.releases as rel (rel.version)}
				<section class="release">
					<Text as="div" weight="bold" variant="code">
						<a class="rel-link" href={rel.url} target="_blank" rel="noopener">v{rel.version}</a>
					</Text>
					{#if rel.body}
						<div class="md">{@html renderMarkdown(rel.body)}</div>
					{:else}
						<Text tone="faint" size="sm">{m.update_release_no_notes()}</Text>
					{/if}
				</section>
			{/each}
		{/if}
		<Text as="div" size="xs" tone="faint">
			<a class="rel-link" href={latestUrl} target="_blank" rel="noopener">{m.update_open_release_page()}</a>
		</Text>
	</div>
{/snippet}

{#snippet footer()}
	{#if isAdmin}
		{#if !selfUpdateReady}
			<Text size="sm" tone="faint">{m.update_no_target_hint()}</Text>
		{:else}
			<Text size="sm" tone="faint">
				{selfUpdateHook ? m.update_hook_hint() : m.update_agent_hint()}
			</Text>
		{/if}
		<Button size="lg" onclick={onclose}>{m.update_not_now()}</Button>
		<Button size="lg" variant="danger" disabled={!selfUpdateReady} onclick={() => (step = 'confirm')}>
			{m.update_button()}
		</Button>
	{:else}
		<Text size="sm" tone="faint">{m.update_ask_admin()}</Text>
		<Button size="lg" onclick={onclose}>{m.update_not_now()}</Button>
	{/if}
{/snippet}

{#snippet confirmBody()}
	<Text as="div" size="xs" weight="bold" tone={selfUpdateHook ? 'success' : 'warn'}>
		{selfUpdateHook ? m.update_hook_badge() : m.update_agent_badge()}
	</Text>
	<Text as="p">{selfUpdateHook ? m.update_confirm_body_hook() : m.update_confirm_body()}</Text>
{/snippet}

{#snippet confirmFooter()}
	<Button size="lg" disabled={launching} onclick={onclose}>{m.update_confirm_no()}</Button>
	<Button size="lg" variant="danger" loading={launching} disabled={launching} onclick={launch}>
		{m.update_confirm_yes()}
	</Button>
{/snippet}

{#snippet runBody()}
	<div class="run">
		{#if phase}
			<Text as="div" weight="bold" tone={phaseTone}>{PHASE_LABEL[phase]()}</Text>
			<Text as="div" size="sm" tone="faint">{run.data?.detail}</Text>
			{#if run.data?.output_tail}
				<Text as="div" size="xs" tone="faint">{m.update_run_output()}</Text>
				<pre class="out">{run.data.output_tail}</pre>
			{/if}
		{:else}
			<Text tone="faint" size="sm">{m.update_run_phase_running()}</Text>
		{/if}
	</div>
{/snippet}

{#snippet runFooter()}
	<Button size="lg" onclick={onclose}>{run.data?.done ? m.common_close() : m.update_not_now()}</Button>
{/snippet}

<style>
	.notes {
		display: flex;
		flex-direction: column;
		gap: var(--sp-4);
		max-height: 60vh;
		overflow: auto;
	}
	.run {
		display: flex;
		flex-direction: column;
		gap: var(--sp-2);
		max-height: 60vh;
		overflow: auto;
	}
	.out {
		margin: 0;
		padding: var(--sp-2);
		background: var(--bg);
		border-radius: var(--r-sm);
		font-size: var(--fs-xs);
		white-space: pre-wrap;
		overflow-wrap: anywhere;
		max-height: 30vh;
		overflow: auto;
	}
	.release {
		display: flex;
		flex-direction: column;
		gap: var(--sp-2);
		padding-bottom: var(--sp-3);
		border-bottom: 1px solid var(--border);
	}
	.release:last-of-type {
		border-bottom: 0;
	}
	.rel-link {
		color: inherit;
	}
	.md {
		font-size: var(--fs-sm);
		line-height: var(--lh-normal);
		overflow-wrap: anywhere;
	}
</style>
