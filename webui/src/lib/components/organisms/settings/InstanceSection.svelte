<script lang="ts">
	// Settings › Instance: what this deployment is (versions, update check) and
	// what this browser spends on it (network, local storage), plus the
	// admin-only server settings — the instance name shown in the header and the
	// machine the self-update agent runs on. Admin values live in
	// `instance_settings` on the server, not in the per-user blob; the name is
	// read back through /version so the header and tab title pick it up on the
	// next refetch.
	import { Button, Input, Select, Text } from '@dorsk/tsumikit';
	import { useQueryClient } from '@tanstack/svelte-query';
	import SettingGroup from '$lib/components/molecules/SettingGroup.svelte';
	import SettingRow from '$lib/components/molecules/SettingRow.svelte';
	import SettingSection from '$lib/components/molecules/SettingSection.svelte';
	import MachinePicker from '$lib/components/molecules/MachinePicker.svelte';
	import NetStatsChip from '$lib/components/molecules/NetStatsChip.svelte';
	import UpdateModal from '$lib/components/organisms/UpdateModal.svelte';
	import StorageSection from './StorageSection.svelte';
	import HarnessUpdateGroup from './HarnessUpdateGroup.svelte';
	import { useVersion, useAllMachines, endpoints, qk } from '$lib/queries';
	import type { SelfUpdateTargetInfo } from '@bindings/SelfUpdateTargetInfo';
	import { toasts } from '$lib/toast.svelte';
	import { m } from '$lib/paraglide/messages';

	let { isAdmin = false }: { isAdmin?: boolean } = $props();

	const version = useVersion();
	const qc = useQueryClient();

	let instanceDraft = $state('');
	let instanceSaving = $state(false);
	$effect(() => {
		instanceDraft = version.data?.instance_name ?? '';
	});
	const instanceDirty = $derived(instanceDraft.trim() !== (version.data?.instance_name ?? ''));

	async function saveInstanceName() {
		instanceSaving = true;
		try {
			const res = await endpoints.updateInstance(instanceDraft.trim() || null);
			instanceDraft = res.name ?? '';
			await qc.invalidateQueries({ queryKey: qk.version });
			toasts.ok(res.name ? m.settings_admin_instance_saved() : m.settings_admin_instance_cleared());
		} catch (e) {
			toasts.error(e instanceof Error ? e.message : String(e));
		} finally {
			instanceSaving = false;
		}
	}

	// The server probes GitHub for a newer release every 6h; this asks it to go
	// now and swaps the cached `/version` payload with the answer, so the
	// header's update arrow reflects the result immediately.
	let updateOpen = $state(false);
	let updateChecking = $state(false);
	async function checkForUpdate() {
		updateChecking = true;
		try {
			const info = await endpoints.refreshVersion();
			qc.setQueryData(qk.version, info);
			toasts.ok(
				info.latest_version
					? m.settings_version_available({ version: info.latest_version })
					: m.settings_version_up_to_date()
			);
		} catch (e) {
			toasts.error(m.settings_version_check_failed({ error: e instanceof Error ? e.message : String(e) }));
		} finally {
			updateChecking = false;
		}
	}

	// Self-update target: which enrolled machine + directory the "Update"
	// button hands the deployment to. The server never learns how cctui is
	// deployed there — the agent reads that machine's own notes.
	const allMachines = useAllMachines(() => isAdmin);
	let suTarget = $state<SelfUpdateTargetInfo | null>(null);
	let suMachine = $state('');
	let suDir = $state('');
	let suAdapter = $state('claude-code');
	let suSaving = $state(false);
	$effect(() => {
		if (!isAdmin) return;
		endpoints
			.selfUpdateTarget()
			.then((info) => {
				suTarget = info;
				suMachine = info.target?.machine_id ?? '';
				suDir = info.target?.working_dir ?? '';
				suAdapter = info.target?.adapter_id ?? 'claude-code';
			})
			.catch(() => {});
	});
	const suDirty = $derived(
		suMachine !== (suTarget?.target?.machine_id ?? '') ||
			suDir.trim() !== (suTarget?.target?.working_dir ?? '') ||
			suAdapter !== (suTarget?.target?.adapter_id ?? 'claude-code')
	);
	const suValid = $derived(suMachine !== '' && suDir.trim() !== '');

	async function saveSelfUpdateTarget(clear = false) {
		suSaving = true;
		try {
			const info = await endpoints.setSelfUpdateTarget(
				clear ? null : { machine_id: suMachine, working_dir: suDir.trim(), adapter_id: suAdapter }
			);
			suTarget = info;
			suMachine = info.target?.machine_id ?? '';
			suDir = info.target?.working_dir ?? '';
			suAdapter = info.target?.adapter_id ?? 'claude-code';
			qc.invalidateQueries({ queryKey: qk.version });
			toasts.ok(clear ? m.settings_self_update_cleared() : m.settings_self_update_saved());
		} catch (e) {
			toasts.error(e instanceof Error ? e.message : String(e));
		} finally {
			suSaving = false;
		}
	}
</script>

<SettingSection
	id="instance"
	icon="⚙"
	title={m.settings_nav_instance()}
	description={m.settings_instance_desc()}
	admin={isAdmin}
>
	{#if isAdmin}
		<SettingGroup title={m.settings_group_server()}>
			<SettingRow
				label={m.settings_admin_instance_name_label()}
				help={m.settings_admin_instance_name_help()}
				server
				admin
			>
				<Input
					bind:value={instanceDraft}
					maxlength={48}
					grow
					placeholder={m.settings_admin_instance_name_placeholder()}
					aria-label={m.settings_admin_instance_name_label()}
					onkeydown={(e: KeyboardEvent) => {
						if (e.key === 'Enter' && instanceDirty && !instanceSaving) saveInstanceName();
					}}
				/>
				<Button disabled={!instanceDirty || instanceSaving} onclick={saveInstanceName}>
					{m.settings_admin_instance_save()}
				</Button>
			</SettingRow>
		</SettingGroup>
	{/if}

	<SettingGroup title={m.settings_group_diagnostics()}>
		<SettingRow label={m.settings_version_title()} help={m.settings_version_check_help()} selfLabelled>
			<div class="ver">
				{#if version.data}
					<Text size="sm" variant="code">srv {version.data.version}</Text>
					<Text size="sm" variant="code">ui {__CLIENT_VERSION__}</Text>
					{#if version.data.latest_version}
						<Button size="sm" variant="ghost" chip onclick={() => (updateOpen = true)}>
							<Text size="sm" variant="code" tone="danger">↑ v{version.data.latest_version}</Text>
						</Button>
					{/if}
				{/if}
				<Button size="sm" disabled={updateChecking} onclick={checkForUpdate}>
					{updateChecking ? m.settings_version_checking() : m.settings_version_check()}
				</Button>
			</div>
		</SettingRow>
		<SettingRow label={m.net_stats_title()} help={m.settings_net_stats_help()} selfLabelled>
			<NetStatsChip />
		</SettingRow>
	</SettingGroup>

	{#if isAdmin}
		<SettingGroup title={m.settings_self_update_label()}>
			<SettingRow
				label={m.settings_self_update_label()}
				help={suTarget?.source === 'env'
					? `${m.settings_self_update_help()} ${m.settings_self_update_from_env()}`
					: m.settings_self_update_help()}
				wide
				selfLabelled
			>
				<div class="su">
					{#if allMachines.data}
						<MachinePicker
							bind:value={suMachine}
							machines={allMachines.data}
							label={m.settings_self_update_machine()}
						/>
					{/if}
					<Input
						bind:value={suDir}
						grow
						placeholder={m.settings_self_update_dir_placeholder()}
						aria-label={m.settings_self_update_dir()}
					/>
					<Select
						value={suAdapter}
						aria-label={m.settings_self_update_adapter()}
						onchange={(e) => (suAdapter = (e.currentTarget as HTMLSelectElement).value)}
					>
						<option value="claude-code">claude-code</option>
						<option value="codex">codex</option>
					</Select>
					<div class="su-actions">
						<Button
							disabled={!suDirty || !suValid || suSaving}
							onclick={() => saveSelfUpdateTarget()}
						>
							{m.settings_admin_instance_save()}
						</Button>
						{#if suTarget?.source === 'settings'}
							<Button variant="ghost" disabled={suSaving} onclick={() => saveSelfUpdateTarget(true)}>
								{m.settings_self_update_clear()}
							</Button>
						{/if}
					</div>
				</div>
			</SettingRow>
		</SettingGroup>
		<HarnessUpdateGroup />
	{/if}

	<StorageSection />
</SettingSection>

{#if updateOpen && version.data?.latest_version}
	<UpdateModal
		latestVersion={version.data.latest_version}
		latestUrl={version.data.latest_url ?? version.data.repo_url}
		selfUpdateReady={version.data.self_update_ready}
		selfUpdateHook={version.data.self_update_hook}
		onclose={() => (updateOpen = false)}
	/>
{/if}

<style>
	.ver {
		display: flex;
		align-items: center;
		justify-content: flex-end;
		gap: var(--sp-2);
		flex-wrap: wrap;
	}
	.su {
		display: flex;
		flex-direction: column;
		gap: var(--sp-2);
		min-width: 0;
	}
	.su-actions {
		display: flex;
		gap: var(--sp-2);
	}
</style>
