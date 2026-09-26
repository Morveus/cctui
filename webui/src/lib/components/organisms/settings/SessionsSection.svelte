<script lang="ts">
	// Settings › Sessions: how the list looks, the docked panels of the Sessions
	// screen, and conversation behaviour (auto-resume, archive shortcut). The
	// per-section sort and the label filter are set on the Sessions page itself,
	// so the description only points there.
	import { Link, SegmentedControl, Select, Switch } from '@dorsk/tsumikit';
	import type { SegmentOption } from '@dorsk/tsumikit';
	import SettingGroup from '$lib/components/molecules/SettingGroup.svelte';
	import SettingRow from '$lib/components/molecules/SettingRow.svelte';
	import SettingSection from '$lib/components/molecules/SettingSection.svelte';
	import { settings } from '$lib/settings.svelte';
	import { m } from '$lib/paraglide/messages';
	import { clampFollowupWhenCold } from '$lib/followup';
	import { GROUP_DIMENSIONS, nextSort } from '../../../../routes/sessions/sessions.logic';

	const sl = $derived(settings.state.sessionList);
	const spawnDock = $derived(settings.spawnDock);
	const statsDock = $derived(settings.statsDock);
	const sessionEmojiPrefix = $derived(settings.sessionEmojiPrefix);
	const autoResume = $derived(settings.autoResumeOnConnectionLoss);

	const viewOptions: SegmentOption[] = [
		{ value: 'list', label: m.settings_view_list() },
		{ value: 'card', label: m.settings_view_cards() }
	];
	const sideOptions: SegmentOption[] = [
		{ value: 'left', label: m.settings_spawn_dock_side_left() },
		{ value: 'right', label: m.settings_spawn_dock_side_right() }
	];
	const followupColdOptions: SegmentOption[] = [
		{ value: 'off', label: m.settings_followup_when_cold_off() },
		{ value: 'offer', label: m.settings_followup_when_cold_offer() },
		{ value: 'default', label: m.settings_followup_when_cold_default() }
	];
</script>

{#snippet sessionsDesc()}
	{m.settings_sessions_desc()}
	<Link href="/sessions">{m.settings_sessions_desc_link()}</Link>.
{/snippet}

{#snippet archiveHelp()}
	{m.settings_archive_shortcut_help()}
{/snippet}

<SettingSection
	id="sessions"
	icon="◰"
	title={m.settings_nav_sessions()}
	descriptionSlot={sessionsDesc}
>
	<SettingGroup title={m.settings_group_list()}>
		<SettingRow label={m.settings_sort_label()}>
			<Select
				value={sl.sort}
				style="width:100%"
				onchange={(e) =>
					settings.setSessionList(
						nextSort(sl, (e.currentTarget as HTMLSelectElement).value as typeof sl.sort)
					)}
			>
				<option value="activity">{m.settings_sort_activity()}</option>
				<option value="created">{m.settings_sort_created()}</option>
				<option value="name">{m.settings_sort_name()}</option>
			</Select>
		</SettingRow>
		<SettingRow label={m.settings_view_label()} selfLabelled>
			<SegmentedControl
				options={viewOptions}
				label={m.settings_view_label()}
				control
				bind:value={() => sl.view, (v) => settings.setSessionList({ view: v as typeof sl.view })}
			/>
		</SettingRow>
		<SettingRow label={m.settings_group_by_label()}>
			<Select
				value={sl.groupBy}
				style="width:100%"
				onchange={(e) =>
					settings.setSessionList({
						groupBy: (e.currentTarget as HTMLSelectElement).value as typeof sl.groupBy
					})}
			>
				{#each GROUP_DIMENSIONS as d (d.value)}
					<option value={d.value}>{d.label}</option>
				{/each}
			</Select>
		</SettingRow>
		<SettingRow label={m.settings_list_width_label()} help={m.settings_list_width_help()}>
			<Select
				value={sl.width}
				style="width:100%"
				onchange={(e) =>
					settings.setSessionList({
						width: (e.currentTarget as HTMLSelectElement).value as typeof sl.width
					})}
			>
				<option value="default">{m.settings_list_width_default()}</option>
				<option value="wide">{m.settings_list_width_wide()}</option>
				<option value="ultra">{m.settings_list_width_ultra()}</option>
				<option value="full">{m.settings_list_width_full()}</option>
			</Select>
		</SettingRow>
		<SettingRow label={m.settings_account_names_label()} help={m.settings_account_names_help()}>
			<Switch
				bind:checked={() => sl.accountNames, (v) => settings.setSessionList({ accountNames: v })}
				label={m.settings_account_names_label()}
			/>
		</SettingRow>
		<SettingRow label={m.settings_session_emoji_label()} help={m.settings_session_emoji_help()} server>
			<Switch
				bind:checked={() => sessionEmojiPrefix, (v) => settings.setSessionEmojiPrefix(v)}
				label={m.settings_session_emoji_label()}
			/>
		</SettingRow>
	</SettingGroup>

	<SettingGroup title={m.settings_spawn_title()}>
		<SettingRow label={m.settings_spawn_dock_label()} help={m.settings_spawn_dock_help()}>
			<Switch
				bind:checked={() => spawnDock.enabled, (v) => settings.setSpawnDock({ enabled: v })}
				label={m.settings_spawn_dock_label()}
			/>
		</SettingRow>
		<SettingRow label={m.settings_spawn_dock_side_label()} disabled={!spawnDock.enabled} selfLabelled>
			<SegmentedControl
				options={sideOptions}
				label={m.settings_spawn_dock_side_label()}
				control
				bind:value={
					() => spawnDock.side, (v) => settings.setSpawnDock({ side: v as typeof spawnDock.side })
				}
			/>
		</SettingRow>
	</SettingGroup>

	<SettingGroup title={m.settings_stats_title()}>
		<SettingRow label={m.settings_stats_dock_label()} help={m.settings_stats_dock_help()}>
			<Switch
				bind:checked={() => statsDock.enabled, (v) => settings.setStatsDock({ enabled: v })}
				label={m.settings_stats_dock_label()}
			/>
		</SettingRow>
		<SettingRow label={m.settings_stats_dock_side_label()} disabled={!statsDock.enabled} selfLabelled>
			<SegmentedControl
				options={sideOptions}
				label={m.settings_stats_dock_side_label()}
				control
				bind:value={
					() => statsDock.side, (v) => settings.setStatsDock({ side: v as typeof statsDock.side })
				}
			/>
		</SettingRow>
	</SettingGroup>

	<SettingGroup title={m.settings_group_conversation()}>
		<SettingRow label={m.settings_auto_resume_label()} help={m.settings_auto_resume_help()}>
			<Switch
				bind:checked={() => autoResume, (v) => settings.setAutoResumeOnConnectionLoss(v)}
				label={m.settings_auto_resume_label()}
			/>
		</SettingRow>
		<SettingRow label={m.settings_archive_shortcut_label()} helpSlot={archiveHelp}>
			<Switch
				bind:checked={() => settings.state.display.archiveShortcut, () => settings.toggleArchiveShortcut()}
				label={m.settings_archive_shortcut_label()}
			/>
		</SettingRow>
		<SettingRow label={m.settings_archive_done_button_label()} help={m.settings_archive_done_button_help()}>
			<Switch
				bind:checked={() => settings.archiveDoneButton, (v) => settings.setArchiveDoneButton(v)}
				label={m.settings_archive_done_button_label()}
			/>
		</SettingRow>
		<SettingRow label={m.settings_pin_first_message_label()} help={m.settings_pin_first_message_help()}>
			<Switch
				bind:checked={() => settings.pinFirstMessage, (v) => settings.setPinFirstMessage(v)}
				label={m.settings_pin_first_message_label()}
			/>
		</SettingRow>
		<SettingRow
			label={m.settings_followup_when_cold_label()}
			help={m.settings_followup_when_cold_help()}
			selfLabelled
		>
			<SegmentedControl
				options={followupColdOptions}
				label={m.settings_followup_when_cold_label()}
				control
				bind:value={
					() => settings.followupWhenCold,
					(v) => settings.setFollowupWhenCold(clampFollowupWhenCold(v))
				}
			/>
		</SettingRow>
		<SettingRow label={m.settings_prefer_followup_label()} help={m.settings_prefer_followup_help()}>
			<Switch
				bind:checked={() => settings.preferFollowupOverFork, (v) => settings.setPreferFollowupOverFork(v)}
				label={m.settings_prefer_followup_label()}
			/>
		</SettingRow>
		<SettingRow label={m.settings_role_tinted_background_label()} help={m.settings_role_tinted_background_help()}>
			<Switch
				bind:checked={() => settings.roleTintedBackground, (v) => settings.setRoleTintedBackground(v)}
				label={m.settings_role_tinted_background_label()}
			/>
		</SettingRow>
	</SettingGroup>
</SettingSection>
