<script lang="ts">
	import type { UserRow } from '@bindings/UserRow';
	import type { MenuItem } from '@dorsk/tsumikit';
	import {
		Avatar,
		ConfirmModal,
		Dot,
		Heading,
		Icon,
		Menu,
		Tabs,
		Text,
		Timestamp
	} from '@dorsk/tsumikit';
	import EditEntityModal from '$lib/components/molecules/EditEntityModal.svelte';
	import { hashHue } from '$lib/format';
	import { type OAuthAccount, useMachines, useTokens, useUserAcls, useUserActions, useUserKeys } from '$lib/queries';
	import { toasts } from '$lib/toast.svelte';
	import { errMessage } from '$lib/api';
	import { m } from '$lib/paraglide/messages';
	import AccessAccountsTab from './AccessAccountsTab.svelte';
	import AccessKeysTab from './AccessKeysTab.svelte';
	import AccessMachinesTab from './AccessMachinesTab.svelte';
	import AccessTokensTab from './AccessTokensTab.svelte';
	import KeyScopesModal from './KeyScopesModal.svelte';
	import { ALL_SCOPES } from './access.logic';

	let {
		user,
		isAdmin,
		isSelf,
		accounts,
		accountsLoading = false,
		onsecret,
		ongone
	}: {
		user: UserRow;
		isAdmin: boolean;
		isSelf: boolean;
		accounts: OAuthAccount[];
		accountsLoading?: boolean;
		onsecret: (title: string, value: string) => void;
		ongone: () => void;
	} = $props();

	const acls = useUserAcls(() => user.id);
	const keys = useUserKeys(() => user.id);
	const machines = useMachines(
		() => user.id,
		() => isAdmin
	);
	const tokens = useTokens(
		() => user.id,
		() => isAdmin
	);
	const actions = useUserActions();
	const guard = (p: Promise<unknown>) => p.catch((e: unknown) => toasts.error(errMessage(e)));

	const ceiling = $derived(new Set(acls.data?.scopes ?? []));
	const canManageKeys = $derived(isAdmin || isSelf);
	const machineCount = $derived((machines.data ?? []).filter((mc) => mc.kind !== 'ephemeral').length);
	const keyCount = $derived((keys.data ?? []).filter((k) => !k.revoked_at).length);
	const tokenCount = $derived((tokens.data ?? []).filter((t) => !t.revoked_at).length);

	const statusText = $derived(
		user.revoked_at ? m.users_badge_revoked() : user.disabled_at ? m.users_badge_disabled() : m.users_badge_active()
	);
	const statusDot = $derived(user.revoked_at || user.disabled_at ? 'dead' : 'active');

	let tab = $state('keys');
	let renameOpen = $state(false);
	let ceilingOpen = $state(false);
	let confirmKind = $state<'revoke' | 'purge' | null>(null);

	// Machines and tokens are admin-only endpoints: a non-admin sees the tabs
	// but cannot open them.
	const tabs = $derived([
		{ id: 'keys', label: m.access_tab_keys() },
		{ id: 'machines', label: m.access_tab_machines({ count: machineCount }), disabled: !isAdmin },
		{ id: 'tokens', label: m.access_tab_tokens({ count: tokenCount }), disabled: !isAdmin },
		{ id: 'accounts', label: m.access_tab_accounts({ count: accounts.length }) }
	]);

	const menuItems = $derived<MenuItem[]>(
		isAdmin
			? [
					{ label: m.access_menu_rename(), icon: 'edit', onselect: () => (renameOpen = true) },
					{
						label: m.access_menu_ceiling(),
						icon: 'lock',
						tag: m.access_tag_admin(),
						tagTone: 'warn',
						onselect: () => (ceilingOpen = true)
					},
					{
						label: user.disabled_at ? m.access_menu_enable() : m.access_menu_disable(),
						icon: 'pause',
						tag: m.access_tag_admin(),
						tagTone: 'warn',
						disabled: !!user.revoked_at,
						onselect: () => toggleDisabled()
					},
					user.revoked_at
						? {
								label: m.access_menu_purge(),
								icon: 'trash',
								danger: true,
								tag: m.access_tag_admin(),
								tagTone: 'warn',
								onselect: () => (confirmKind = 'purge')
							}
						: {
								label: m.access_menu_revoke(),
								icon: 'x-circle',
								danger: true,
								tag: m.access_tag_admin(),
								tagTone: 'warn',
								onselect: () => (confirmKind = 'revoke')
							}
				]
			: []
	);

	function rename(name: string | null) {
		if (name) guard(actions.rename(user.id, name).then(() => toasts.ok(m.users_renamed())));
	}
	function toggleDisabled() {
		const disabled = !user.disabled_at;
		guard(
			actions
				.setDisabled(user.id, disabled)
				.then(() =>
					toasts.ok(
						disabled ? m.users_user_disabled({ name: user.name }) : m.users_user_enabled({ name: user.name })
					)
				)
		);
	}
	function setCeiling(scopes: string[]) {
		guard(actions.setUserScopes(user.id, scopes));
	}
	function runConfirm() {
		const purge = confirmKind === 'purge';
		guard(
			(purge ? actions.purgeUser(user.id) : actions.revoke(user.id)).then(() => {
				toasts.ok(purge ? m.users_user_deleted() : m.users_revoked());
				if (purge) ongone();
			})
		);
		confirmKind = null;
	}
</script>

<div class="detail">
	<header class="dhead">
		<Avatar name={user.name} hue={hashHue(user.name)} size={40} decorative />
		<div class="who">
			<div class="line">
				<Heading level={2} size="lg">{user.name}</Heading>
				<span class="status">
					<Dot status={statusDot} />
					<Text size="sm" tone="muted">{statusText}</Text>
					{#if user.last_seen_at && !user.revoked_at}
						<Text size="xs" tone="faint">· {m.access_last_seen()}</Text>
						<Timestamp value={user.last_seen_at} mode="relative" size="xs" tone="faint" />
					{/if}
				</span>
			</div>
			<div class="meta">
				<Text size="xs" tone="faint">{m.access_since()}</Text>
				<Timestamp value={user.created_at} mode="short-iso" size="xs" tone="faint" />
				<Text size="xs" tone="faint">
					· {m.access_ceiling()}
					{[...ALL_SCOPES].filter((s) => ceiling.has(s)).join(' · ') || m.access_ceiling_none()}
					{#if isAdmin}
						· {m.access_count_machines({ count: machineCount })}
					{/if}
					· {m.access_count_keys({ count: keyCount })}
					{#if isAdmin}
						· {m.access_count_tokens({ count: tokenCount })}
					{/if}
				</Text>
			</div>
		</div>
		<div class="spacer"></div>
		{#if menuItems.length}
			<Menu label={m.common_actions()} items={menuItems} placement="bottom-end" box="sm">
				{#snippet trigger()}<Icon name="more" size={16} />{/snippet}
			</Menu>
		{/if}
	</header>

	<Tabs {tabs} bind:value={tab} label={m.access_tabs_label()}>
		{#snippet panel(id)}
			<div data-journey="tab" data-journey-key={id}>
			{#if id === 'keys'}
				<AccessKeysTab userId={user.id} {ceiling} canManage={canManageKeys && !user.revoked_at} {onsecret} />
			{:else if id === 'machines'}
				<AccessMachinesTab userId={user.id} canManage={isAdmin} />
			{:else if id === 'tokens'}
				<AccessTokensTab
					userId={user.id}
					userName={user.name}
					canManage={isAdmin && !user.revoked_at}
					{onsecret}
				/>
			{:else}
				<AccessAccountsTab {accounts} loading={accountsLoading} />
			{/if}
			</div>
		{/snippet}
	</Tabs>
</div>

{#if renameOpen}
	<EditEntityModal
		title={m.users_rename_user()}
		fieldLabel={m.users_field_user_name()}
		name={user.name}
		onsave={(name) => rename(name)}
		onclose={() => (renameOpen = false)}
	/>
{/if}

{#if ceilingOpen}
	<KeyScopesModal
		title={m.access_menu_ceiling()}
		scopes={[...ceiling]}
		ceiling={new Set(ALL_SCOPES)}
		help={m.users_scopes_ceiling_help()}
		onsave={(_l, scopes) => setCeiling(scopes)}
		onclose={() => (ceilingOpen = false)}
	/>
{/if}

{#if confirmKind}
	<ConfirmModal
		open
		tone="danger"
		title={confirmKind === 'purge' ? m.access_menu_purge() : m.access_menu_revoke()}
		message={confirmKind === 'purge'
			? m.users_confirm_purge_user({ name: user.name })
			: m.users_confirm_revoke_user({ name: user.name })}
		confirmLabel={confirmKind === 'purge' ? m.users_delete_permanently() : m.users_revoke()}
		onconfirm={runConfirm}
		oncancel={() => (confirmKind = null)}
	/>
{/if}

<style>
	.detail {
		display: flex;
		flex-direction: column;
		gap: var(--sp-4);
		padding: var(--sp-1) 0 var(--sp-4) var(--sp-4);
	}
	/* Stacked under the breakpoint the pane already spans the page: no inset. */
	@media (max-width: 47.999rem) {
		.detail {
			padding-left: 0;
		}
	}
	.dhead {
		display: flex;
		align-items: center;
		gap: var(--sp-3);
	}
	.who {
		min-width: 0;
	}
	.line {
		display: flex;
		align-items: center;
		gap: var(--sp-2);
	}
	.status {
		display: flex;
		align-items: center;
		gap: var(--sp-1);
	}
	.meta {
		display: flex;
		align-items: center;
		gap: 4px;
		flex-wrap: wrap;
	}
	.spacer {
		flex: 1;
	}
</style>
