<script lang="ts">
	// The session kit: harness · account · model · effort · permission mode.
	// One organism, so the no-profile spawn form and the profile editor are the
	// same fields — the editor only adds a name row and its save actions.
	import type { AccountPoolView } from '@bindings/AccountPoolView';
	import type { AccountUsageEntry, OAuthAccount } from '$lib/queries';
	import { useCodexModels, useMergedCodexModels } from '$lib/queries';
	import { AutoGrid, Field, OptionButton, Select, Switch, Text } from '@dorsk/tsumikit';
	import type { SelectOption } from '@dorsk/tsumikit';
	import BrandLogo from '$lib/components/atoms/BrandLogo.svelte';
	import CodexModelsRefresh from '$lib/components/molecules/CodexModelsRefresh.svelte';
	import EffortSlider from './EffortSlider.svelte';
	import PermissionModes from './PermissionModes.svelte';
	import { declaredModelOptions, preferCatalog, withDeclaredModels } from '$lib/harnessModels';
	import {
		accountBacksAdapter,
		accountPickOptions,
		adapterLabel,
		allAdapters,
		claudeEfforts,
		claudeModels,
		codexEffortsFor,
		codexModelsFor,
		isCompatibleProvider,
		NO_ACCOUNT,
		POOL_PREFIX,
		providerForAdapter,
		withAliasTargets
	} from './options';
	import { accountById, accountUsedPct, type ProfileSpec } from './profiles';
	import { m } from '$lib/paraglide/messages';

	let {
		draft = $bindable(),
		accounts,
		pools = [],
		usage,
		machineId,
		/** Distinguishes the effort slider's ids when several kits are mounted. */
		idSuffix = 'new'
	}: {
		draft: ProfileSpec;
		accounts: OAuthAccount[];
		pools?: AccountPoolView[];
		usage: AccountUsageEntry[];
		machineId: string;
		idSuffix?: string;
	} = $props();

	const account = $derived(accountById(accounts, draft.account_id));
	const provider = $derived(providerForAdapter(account, draft.harness));
	const usesAccountModels = $derived(!!provider && isCompatibleProvider(provider.provider));

	// One picker value space: '' Auto · the no-account sentinel · pool ids
	// behind POOL_PREFIX · account ids.
	const accountPick = $derived(
		draft.no_account
			? NO_ACCOUNT
			: draft.pool_id
				? `${POOL_PREFIX}${draft.pool_id}`
				: (draft.account_id ?? '')
	);
	function setAccountPick(v: string) {
		draft.no_account = v === NO_ACCOUNT;
		draft.pool_id = v.startsWith(POOL_PREFIX) ? v.slice(POOL_PREFIX.length) : null;
		draft.account_id = v && v !== NO_ACCOUNT && !v.startsWith(POOL_PREFIX) ? v : null;
	}
	const accountOptions = $derived<SelectOption[]>(
		accountPickOptions({
			accounts,
			pools,
			harness: draft.harness,
			usedPct: (id) => accountUsedPct(usage, id)
		})
	);

	const machineCodex = useCodexModels(() => (draft.harness === 'codex' ? machineId : ''));
	const mergedCodex = useMergedCodexModels(() => draft.harness === 'codex');
	const codexCatalog = $derived(preferCatalog(machineCodex.data, mergedCodex.data));
	const modelOptions = $derived.by<SelectOption[]>(() => {
		const list = usesAccountModels
			? declaredModelOptions(provider?.models)
			: withDeclaredModels(
					provider?.models,
					draft.harness === 'codex'
						? codexModelsFor(codexCatalog)
						: withAliasTargets(claudeModels, provider?.model_aliases)
				);
		const out: SelectOption[] = list.map((o) => ({
			value: o.v,
			label: o.v ? o.label : m.spawn_model_default(),
			hint: o.hint,
			disabled: o.disabled
		}));
		if (!out.some((o) => o.value === '')) out.unshift({ value: '', label: m.spawn_model_default() });
		const current = draft.model_alias ?? '';
		if (current && !out.some((o) => o.value === current)) out.push({ value: current, label: current });
		return out;
	});
	const efforts = $derived(
		draft.harness === 'codex' ? codexEffortsFor(codexCatalog, draft.model_alias ?? '') : claudeEfforts
	);

	function pickHarness(harness: string) {
		if (harness === draft.harness) return;
		draft.harness = harness;
		draft.model_alias = null;
		draft.effort = null;
		draft.service_tier = null;
		if (account && !accountBacksAdapter(account, harness)) draft.account_id = null;
	}
</script>

<div class="kit">
	<Field label={m.spawn_field_harness()}>
		<AutoGrid min="8rem" maxCols={2} gap="var(--sp-2)" role="radiogroup" aria-label={m.spawn_profile_harness_aria()}>
			{#each allAdapters as ad (ad)}
				<OptionButton
					row
					selected={draft.harness === ad}
					role="radio"
					aria-checked={draft.harness === ad}
					style="--opt-accent: {ad === 'codex' ? 'var(--c-blue)' : 'var(--c-amber)'}"
					onclick={() => pickHarness(ad)}
				>
					<BrandLogo adapter={ad} size={18} />
					<Text>{adapterLabel(ad)}</Text>
				</OptionButton>
			{/each}
		</AutoGrid>
	</Field>

	<AutoGrid min="8rem" maxCols={2} gap="var(--sp-2)">
		<Field label={m.spawn_account_label()} for="sp-kit-account-{idSuffix}">
			<Select
				id="sp-kit-account-{idSuffix}"
				options={accountOptions}
				bind:value={() => accountPick, setAccountPick}
			/>
		</Field>
		<Field label={m.spawn_field_model()} for="sp-kit-model-{idSuffix}">
			<div class="model">
				<div class="grow">
					<Select
						id="sp-kit-model-{idSuffix}"
						options={modelOptions}
						bind:value={() => draft.model_alias ?? '', (v) => (draft.model_alias = v || null)}
					/>
				</div>
				<!-- Re-reads every account's catalog upstream, for a model that
				     appeared since the last refresh. -->
				{#if draft.harness === 'codex' && machineId}
					<CodexModelsRefresh {machineId} size={14} />
				{/if}
			</div>
		</Field>
	</AutoGrid>

	<EffortSlider
		id="sp-kit-effort-{idSuffix}"
		levels={efforts}
		current={draft.effort ?? ''}
		onset={(v) => (draft.effort = v || null)}
	/>

	{#if draft.harness === 'codex'}
		<div class="fast">
			<Switch
				id="sp-kit-fast-{idSuffix}"
				bind:checked={() => draft.service_tier === 'fast',
				(v: boolean) => (draft.service_tier = v ? 'fast' : null)}
				label={m.spawn_fast_label()}
				labelVisible
				size="sm"
			/>
			<Text as="div" size="xs" tone="faint">{m.spawn_fast_hint()}</Text>
		</div>
	{/if}

	<PermissionModes
		value={draft.permission_mode ?? null}
		onpick={(v) => (draft.permission_mode = v)}
	/>
</div>

<style>
	.kit {
		display: flex;
		flex-direction: column;
		gap: var(--sp-3);
	}
	.fast {
		display: flex;
		flex-direction: column;
		gap: var(--sp-1);
	}
	.model {
		display: flex;
		align-items: center;
		gap: var(--sp-1);
		min-width: 0;
	}
	.grow {
		flex: 1;
		min-width: 0;
	}
</style>
