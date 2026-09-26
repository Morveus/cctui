import { createQuery, useQueryClient } from "@tanstack/svelte-query";
import type { AccountRedirect } from "@bindings/AccountRedirect";
import type { PutRedirectRequest } from "@bindings/PutRedirectRequest";
import type { CreatePoolRequest } from "@bindings/CreatePoolRequest";
import type { UpdatePoolRequest } from "@bindings/UpdatePoolRequest";
import { api } from "../api";
import { endpoints } from "./endpoints";
import { qk } from "./keys";
import type {
  AccountUsageEntry,
  UsageHistory,
  UsageWindowCloses,
  CreateAccount,
  CreateProvider,
  GrantShare,
  OAuthFinish,
  UpdateAccount,
  UpdateProvider,
} from "./types";

export const useAccounts = (enabled: () => boolean = () => true) =>
  createQuery(() => ({
    queryKey: ["accounts"],
    queryFn: endpoints.accounts,
    enabled: enabled(),
  }));

export const useRedirects = (enabled: () => boolean = () => true) =>
  createQuery(() => ({
    queryKey: ["redirects"],
    queryFn: endpoints.redirects,
    enabled: enabled(),
  }));

export type RedirectChip = {
  id: string;
  family: string;
  targetName: string;
  until: string | null;
};

/** Redirect rules for one account, `to_account` resolved to a name. A rule
 *  without a target is not a redirect and is dropped. */
export function redirectChipsFor(
  rules: readonly AccountRedirect[],
  accounts: readonly { id: string; name: string }[],
  accountId: string,
): RedirectChip[] {
  return rules
    .filter((r) => r.to_account !== null && r.from_account === accountId)
    .map((r) => ({
      id: r.id,
      family: r.family,
      targetName: accounts.find((t) => t.id === r.to_account)?.name ?? "…",
      until: r.expires_at ?? null,
    }));
}

export function useRedirectChips(enabled: () => boolean = () => true) {
  const redirects = useRedirects(enabled);
  const accounts = useAccounts(enabled);
  return {
    chipsFor: (accountId: string): RedirectChip[] =>
      redirectChipsFor(redirects.data ?? [], accounts.data ?? [], accountId),
  };
}

/** Set/clear launch-time redirect rules; both invalidate the rules query. */
export function useRedirectActions() {
  const qc = useQueryClient();
  const invalidate = () => qc.invalidateQueries({ queryKey: ["redirects"] });
  return {
    put: async (accountId: string, body: PutRedirectRequest) => {
      const r = await endpoints.putRedirect(accountId, body);
      invalidate();
      return r;
    },
    remove: async (id: string) => {
      await endpoints.deleteRedirect(id);
      invalidate();
    },
  };
}

/** The caller's account pools with their membership. A pool is the durable
 *  "these accounts are interchangeable" statement that bounds both auto-binding
 *  and mid-session failover; see the accounts screen's Pools tab. */
export const useAccountPools = (enabled: () => boolean = () => true) =>
  createQuery(() => ({
    queryKey: ["account-pools"],
    queryFn: endpoints.accountPools,
    enabled: enabled(),
  }));

/** Every pool's usage windows aggregated per provider family, for the pool
 *  zones and the stats panel. Same slow cadence as `useAllAccountsUsage`:
 *  server-side the rows come from the same per-provider cache. */
export const useAccountPoolsUsage = (enabled: () => boolean = () => true) =>
  createQuery(() => ({
    queryKey: ["account-pools-usage"],
    queryFn: endpoints.accountPoolsUsage,
    enabled: enabled(),
    staleTime: 180_000,
    refetchInterval: 180_000,
    refetchOnWindowFocus: false,
    retry: false,
  }));

/** Create / edit / delete pools; every call invalidates the pools query and
 *  the pool usage aggregate, whose membership just changed. */
export function useAccountPoolActions() {
  const qc = useQueryClient();
  const invalidate = () => {
    qc.invalidateQueries({ queryKey: ["account-pools"] });
    qc.invalidateQueries({ queryKey: ["account-pools-usage"] });
  };
  return {
    /** Replace several pools' memberships in order (an account leaving one
     *  pool before joining another), invalidating once at the end. */
    move: async (changes: { poolId: string; accounts: string[] }[]) => {
      for (const c of changes) {
        await endpoints.updateAccountPool(c.poolId, { accounts: c.accounts });
      }
      if (changes.length) invalidate();
    },
    create: async (body: CreatePoolRequest) => {
      const r = await endpoints.createAccountPool(body);
      invalidate();
      return r;
    },
    update: async (id: string, body: UpdatePoolRequest) => {
      const r = await endpoints.updateAccountPool(id, body);
      invalidate();
      return r;
    },
    remove: async (id: string) => {
      await endpoints.deleteAccountPool(id);
      invalidate();
    },
  };
}

/** A session's mid-run account moves. Lazy: only fetched while a session's
 *  drawer is open, since the vast majority of sessions never move at all. */
export const useSessionRebinds = (
  sessionId: () => string,
  enabled: () => boolean = () => true,
) =>
  createQuery(() => ({
    queryKey: ["session-rebinds", sessionId()],
    queryFn: () => endpoints.sessionRebinds(sessionId()),
    enabled: enabled(),
    retry: false,
  }));

/** Safety net, not the primary refresh: freshness comes from the server's
 *  `account_usage` push. Matches the server-side cache TTL, because Anthropic's
 *  usage endpoint rate-limits per access token — do not shorten it. */
export const USAGE_POLL_MS = 180_000;

/** The caches a pushed usage row patches. Both are fed by one server-side
 *  per-account cache entry, so they must never be invalidated independently. */
export const usageKeys = {
  one: (accountId: string) => ["account-usage", accountId],
  all: () => ["accounts-usage"],
};

/** Per-account subscription usage. Lazy: only fetched while the accounts view
 *  is mounted (caller gates `enabled`). Codex accounts return `null`. */
export const useAccountUsage = (
  accountId: () => string,
  enabled: () => boolean = () => true,
) =>
  createQuery(() => ({
    queryKey: usageKeys.one(accountId()),
    queryFn: () => endpoints.accountUsage(accountId()),
    enabled: enabled(),
    staleTime: USAGE_POLL_MS,
    refetchInterval: USAGE_POLL_MS,
    refetchOnWindowFocus: false,
    retry: false,
  }));

/** Every owned provider's usage windows in one request, for the header
 *  batteries. Same slow cadence as `useAccountUsage`; server-side each row is
 *  served by the same per-provider cache, so the two never double-hit upstream. */
export const useAllAccountsUsage = (enabled: () => boolean = () => true) =>
  createQuery(() => ({
    queryKey: usageKeys.all(),
    queryFn: () => api.get<AccountUsageEntry[]>("/accounts/usage"),
    enabled: enabled(),
    staleTime: USAGE_POLL_MS,
    refetchInterval: USAGE_POLL_MS,
    refetchOnWindowFocus: false,
    retry: false,
  }));

/** Sampled utilization of one credential since `from` (ISO). */
export const useUsageHistory = (
  accountId: () => string | null,
  windowKey: () => string,
  from: () => string,
) =>
  createQuery(() => ({
    queryKey: ["account-usage-history", accountId(), windowKey(), from()],
    queryFn: () =>
      api.get<UsageHistory>(`/accounts/${accountId()}/usage/history`, {
        window: windowKey(),
        from: from(),
      }),
    enabled: !!accountId(),
    staleTime: USAGE_POLL_MS,
    refetchInterval: USAGE_POLL_MS,
    refetchOnWindowFocus: false,
    retry: false,
  }));

/** Closed quota windows of every owned credential since `from` (ISO). */
export const useUsageCloses = (from: () => string) =>
  createQuery(() => ({
    queryKey: ["account-usage-closes", from()],
    queryFn: () => api.get<UsageWindowCloses>("/accounts/usage/closes", { from: from() }),
    staleTime: USAGE_POLL_MS,
    refetchInterval: USAGE_POLL_MS,
    refetchOnWindowFocus: false,
    retry: false,
  }));

/** Claim a usage-limit reset on a provider credential and refresh its usage
 *  row right away so the bars reflect the claim. */
export function useLimitReset() {
  const qc = useQueryClient();
  return async (accountId: string, creditId?: string | null) => {
    const r = await endpoints.accountLimitReset(accountId, creditId);
    // The header strip reads `accounts-usage`; invalidating only the per-account
    // key leaves it showing the pre-claim figure until its next poll.
    qc.invalidateQueries({ queryKey: usageKeys.one(accountId) });
    qc.invalidateQueries({ queryKey: usageKeys.all() });
    return r;
  };
}

/** Who an account is shared with. Owner-scoped: the server 404s the
 *  list for a non-owner, so callers gate `enabled` to the account owner/admin. */
export const useAccountShares = (
  accountId: () => string,
  enabled: () => boolean = () => true,
) =>
  createQuery(() => ({
    queryKey: qk.accountShares(accountId()),
    queryFn: () => endpoints.accountShares(accountId()),
    enabled: enabled(),
    retry: false,
  }));

/** Who a resource is shared with, for any shareable kind. Owner-scoped
 *  server-side (404s for a non-owner), so callers gate `enabled` accordingly. */
export const useResourceShares = (
  resourceType: () => string,
  id: () => string,
  enabled: () => boolean = () => true,
) =>
  createQuery(() => ({
    queryKey: qk.resourceShares(resourceType(), id()),
    queryFn: () => endpoints.resourceShares(resourceType(), id()),
    enabled: enabled(),
    retry: false,
  }));

/** Grant/revoke actions for generic resource sharing; each invalidates
 *  that resource's shares query. */
export function useResourceShareActions() {
  const qc = useQueryClient();
  return {
    grant: async (resourceType: string, id: string, body: GrantShare) => {
      const r = await endpoints.grantResourceShare(resourceType, id, body);
      qc.invalidateQueries({ queryKey: qk.resourceShares(resourceType, id) });
      return r;
    },
    revoke: async (resourceType: string, id: string, userId: string) => {
      await endpoints.revokeResourceShare(resourceType, id, userId);
      qc.invalidateQueries({ queryKey: qk.resourceShares(resourceType, id) });
    },
  };
}

/** CRUD for the caller's own OAuth accounts. Invalidates the accounts
 *  list after a mutation. */
export function useAccountActions() {
  const qc = useQueryClient();
  const inval = () => qc.invalidateQueries({ queryKey: ["accounts"] });
  return {
    create: async (body: CreateAccount) => {
      const r = await endpoints.createAccount(body);
      inval();
      return r;
    },
    // "Sign in with Claude": start returns the authorize URL the
    // page opens in a new tab; finish exchanges the pasted code for tokens
    // and creates the account (no inval needed on start, only on finish).
    oauthStart: (provider: string, userId?: string, accountId?: string) =>
      endpoints.oauthStart(provider, userId, accountId),
    oauthFinish: async (body: OAuthFinish) => {
      const r = await endpoints.oauthFinish(body);
      inval();
      return r;
    },
    update: async (id: string, body: UpdateAccount) => {
      const r = await endpoints.updateAccount(id, body);
      inval();
      // The pool weight feeds the pool aggregate.
      if (body.pool_weight !== undefined) {
        qc.invalidateQueries({ queryKey: ["account-pools-usage"] });
      }
      return r;
    },
    updateProvider: async (accountId: string, providerId: string, body: UpdateProvider) => {
      const r = await endpoints.updateProvider(accountId, providerId, body);
      inval();
      return r;
    },
    addProvider: async (accountId: string, body: CreateProvider) => {
      const r = await endpoints.addProvider(accountId, body);
      inval();
      return r;
    },
    removeProvider: async (accountId: string, providerId: string) => {
      await endpoints.deleteProvider(accountId, providerId);
      inval();
    },
    moveProvider: async (accountId: string, providerId: string, targetAccountId: string) => {
      const r = await endpoints.moveProvider(accountId, providerId, targetAccountId);
      inval();
      return r;
    },
    remove: async (id: string) => {
      await endpoints.deleteAccount(id);
      inval();
    },
    // Sharing: grant/revoke invalidate that account's shares query.
    grantShare: async (id: string, body: GrantShare) => {
      const r = await endpoints.grantShare(id, body);
      qc.invalidateQueries({ queryKey: qk.accountShares(id) });
      return r;
    },
    revokeShare: async (id: string, userId: string) => {
      await endpoints.revokeShare(id, userId);
      qc.invalidateQueries({ queryKey: qk.accountShares(id) });
    },
  };
}
