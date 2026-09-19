import { api } from "../api";
import type { SessionListResponse } from "@bindings/SessionListResponse";
import type { PoolUsageView } from "@bindings/PoolUsageView";
import type { SessionStats } from "@bindings/SessionStats";
import type { TokenUsageWindows } from "@bindings/TokenUsageWindows";
import type { UsageAnalytics } from "@bindings/UsageAnalytics";
import type { SessionListItem } from "@bindings/SessionListItem";
import type { AgentEvent } from "@bindings/AgentEvent";
import type { MessagePin } from "@bindings/MessagePin";
import type { SpawnRequest } from "@bindings/SpawnRequest";
import type { SessionProfile } from "@bindings/SessionProfile";
import type { CreateProfileRequest } from "@bindings/CreateProfileRequest";
import type { UpdateProfileRequest } from "@bindings/UpdateProfileRequest";
import type { ReorderProfilesRequest } from "@bindings/ReorderProfilesRequest";
import type { SpawnResponse } from "@bindings/SpawnResponse";
import type { ForkRequest } from "@bindings/ForkRequest";
import type { ForkResponse } from "@bindings/ForkResponse";
import type { StageFilesResponse } from "@bindings/StageFilesResponse";
import type { DispatchRequest } from "@bindings/DispatchRequest";
import type { DispatchResponse } from "@bindings/DispatchResponse";
import type { UserRow } from "@bindings/UserRow";
import type { MachineRow } from "@bindings/MachineRow";
import type { MachineResourcesRow } from "@bindings/MachineResourcesRow";
import type { MemCeilingRequest } from "@bindings/MemCeilingRequest";
import type { UserTokenRow } from "@bindings/UserTokenRow";
import type { UserAclsResponse } from "@bindings/UserAclsResponse";
import type { ApiKeyRow } from "@bindings/ApiKeyRow";
import type { PasskeyAssertion } from "@bindings/PasskeyAssertion";
import type { PasskeyAutoPromptRequest } from "@bindings/PasskeyAutoPromptRequest";
import type { PasskeyChallenge } from "@bindings/PasskeyChallenge";
import type { PasskeyListResponse } from "@bindings/PasskeyListResponse";
import type { PasskeyRegisterFinish } from "@bindings/PasskeyRegisterFinish";
import type { PasskeyRow } from "@bindings/PasskeyRow";
import type { PasskeyTestResult } from "@bindings/PasskeyTestResult";
import type { RelabelPasskeyRequest } from "@bindings/RelabelPasskeyRequest";
import type { VersionInfo } from "@bindings/VersionInfo";
import type { GitInfo } from "@bindings/GitInfo";
import type { InstanceInfo } from "@bindings/InstanceInfo";
import type { ChangelogResponse } from "@bindings/ChangelogResponse";
import type { SelfUpdateResponse } from "@bindings/SelfUpdateResponse";
import type { SelfUpdateRun } from "@bindings/SelfUpdateRun";
import type { SelfUpdateTarget } from "@bindings/SelfUpdateTarget";
import type { SelfUpdateTargetInfo } from "@bindings/SelfUpdateTargetInfo";
import type { SelfUpdateTargetRequest } from "@bindings/SelfUpdateTargetRequest";
import type { InstanceUpdateRequest } from "@bindings/InstanceUpdateRequest";
import type { MeResponse } from "@bindings/MeResponse";
import type { CapabilitiesResponse } from "@bindings/CapabilitiesResponse";
import type { LangfuseSessionUsage } from "@bindings/LangfuseSessionUsage";
import type { CodexModelCatalog } from "@bindings/CodexModelCatalog";
import type { LabelListResponse } from "@bindings/LabelListResponse";
import type { RescrubRequest } from "@bindings/RescrubRequest";
import type { RescrubReport } from "@bindings/RescrubReport";
import type { SettingsCatalogResponse } from "@bindings/SettingsCatalogResponse";
import type { SessionDiagnoseResponse } from "@bindings/SessionDiagnoseResponse";
import type { AccountRedirect } from "@bindings/AccountRedirect";
import type { AccountPoolView } from "@bindings/AccountPoolView";
import type { CreatePoolRequest } from "@bindings/CreatePoolRequest";
import type { UpdatePoolRequest } from "@bindings/UpdatePoolRequest";
import type { SessionRebind } from "@bindings/SessionRebind";
import type { PutRedirectRequest } from "@bindings/PutRedirectRequest";
import type { Bookmark } from "@bindings/Bookmark";
import type { CreateBookmark } from "@bindings/CreateBookmark";
import type { UpdateBookmark } from "@bindings/UpdateBookmark";
import { SYSTEM_MACHINE_KINDS } from "./keys";
import type {
  AccountProvider,
  AccountUsage,
  LimitResetResponse,
  CreateAccount,
  CreateProvider,
  GrantShare,
  OAuthAccount,
  OAuthFinish,
  OAuthStartResponse,
  ResourceShareInfo,
  SessionAttachment,
  SessionBinding,
  ShareInfo,
  UpdateAccount,
  UpdateProvider,
} from "./types";
import type {
  EnrollDispatcherResponse,
  RenameDispatcher,
  UserDispatcher,
} from "./dispatchers";

/** Raw typed fetchers — also usable outside of components. */
export const endpoints = {
  version: () => api.get<VersionInfo>("/version"),
  /** Probe upstream for a newer release now rather than waiting out the
   *  server's 6h background interval; answers the same shape as `/version`. */
  refreshVersion: () => api.post<VersionInfo>("/version/refresh"),
  /** Server-wide deployment name (admin). Empty clears it; read back on `/version`. */
  updateInstance: (name: string | null) =>
    api.put<InstanceInfo>("/admin/instance", { name } satisfies InstanceUpdateRequest),
  /** Release notes of every upstream release newer than this server, as the
   *  background probe last saw them (no network call from the browser). */
  changelog: () => api.get<ChangelogResponse>("/version/changelog"),
  /** Deploy the newer release on the configured self-update machine (admin):
   *  the machine's own update hook when it has one, a YOLO agent otherwise —
   *  the answer's `mode` says which. `409` when up to date, unconfigured, or
   *  one is already running. */
  selfUpdate: () => api.post<SelfUpdateResponse>("/version/self-update"),
  /** The most recent update-hook run, or `null` on a deployment that has never
   *  run one. Polled while an update is in flight. */
  selfUpdateStatus: () => api.get<SelfUpdateRun | null>("/version/self-update"),
  /** Machine + directory the self-update agent runs on (admin). */
  selfUpdateTarget: () => api.get<SelfUpdateTargetInfo>("/admin/instance/self-update"),
  /** Set (or clear with `null`) the self-update target (admin). */
  setSelfUpdateTarget: (target: SelfUpdateTarget | null) =>
    api.put<SelfUpdateTargetInfo>("/admin/instance/self-update", {
      target,
    } satisfies SelfUpdateTargetRequest),
  /** Which optional integrations this server has, and whether each is live.
   *  Drives capability-gated UI: the lazy `/github` route + nav. */
  capabilities: () => api.get<CapabilitiesResponse>("/capabilities"),
  /** Who the stored bearer token resolves to. */
  me: () => api.get<MeResponse>("/me"),
  /** Passkeys enrolled on the caller's account. */
  passkeys: () => api.get<PasskeyListResponse>("/passkeys"),
  /** Begin enrolling a passkey; the options go to `navigator.credentials.create()`. */
  passkeyRegisterStart: () => api.post<PasskeyChallenge>("/passkeys/register/start"),
  /** Store the credential the authenticator just produced. */
  passkeyRegisterFinish: (body: PasskeyRegisterFinish) =>
    api.post<PasskeyRow>("/passkeys/register/finish", body),
  /** Begin a "does my key answer?" check; mints nothing. */
  passkeyTestStart: () => api.post<PasskeyChallenge>("/passkeys/test/start"),
  passkeyTestFinish: (body: PasskeyAssertion) =>
    api.post<PasskeyTestResult>("/passkeys/test/finish", body),
  renamePasskey: (id: string, label: string) =>
    api.patch<void>(`/passkeys/${id}`, { label } satisfies RelabelPasskeyRequest),
  revokePasskey: (id: string) => api.del<void>(`/passkeys/${id}`),
  /** Server-wide (admin): read the passkey ceremony as soon as the login
   *  screen opens, instead of waiting for a click. */
  setPasskeyAutoPrompt: (auto_prompt: boolean) =>
    api.put<void>("/admin/passkeys/auto-prompt", {
      auto_prompt,
    } satisfies PasskeyAutoPromptRequest),
  sessions: (archived: boolean) =>
    api.get<SessionListResponse>("/sessions", {
      include_archived: archived || undefined,
    }),
  /** Aggregate session counts for the Overview — correct past the list's
   * 25-row display cap (the list-derived counts are not). */
  sessionStats: (timezone = "UTC") =>
    api.get<SessionStats>(`/sessions/stats?timezone=${encodeURIComponent(timezone)}`),
  /** Every label known to the server — feeds the picker + filter. */
  labels: () => api.get<LabelListResponse>("/labels"),
  /** Saved messages, newest first. `q` filters over title/body/note. */
  bookmarks: (q?: string, limit?: number, before?: string) =>
    api.get<Bookmark[]>("/bookmarks", {
      q: q && q.trim() !== "" ? q : undefined,
      limit,
      before,
    }),
  createBookmark: (body: CreateBookmark) =>
    api.post<Bookmark>("/bookmarks", body),
  updateBookmark: (id: string, body: UpdateBookmark) =>
    api.patch<Bookmark>(`/bookmarks/${id}`, body),
  deleteBookmark: (id: string) => api.del<void>(`/bookmarks/${id}`),
  /** The caller's spawn profiles, oldest first. */
  profiles: () => api.get<SessionProfile[]>("/profiles"),
  createProfile: (body: CreateProfileRequest) =>
    api.post<SessionProfile>("/profiles", body),
  updateProfile: (id: string, body: UpdateProfileRequest) =>
    api.patch<SessionProfile>(`/profiles/${id}`, body),
  deleteProfile: (id: string) => api.del<void>(`/profiles/${id}`),
  reorderProfiles: (body: ReorderProfilesRequest) =>
    api.put<SessionProfile[]>("/profiles/order", body),
  /** Token totals across rolling windows for the Overview. `tzOffset` is
   * `Date.getTimezoneOffset()` — only used to anchor "today" to local midnight. */
  tokenStats: (tzOffset: number) =>
    api.get<TokenUsageWindows>("/sessions/stats/tokens", {
      tz_offset: tzOffset,
    }),
  /** Overview usage analytics: tokens-over-time buckets, per-model
   * breakdown, and an hour-of-week activity heatmap. `days` sets the range +
   * bucket granularity; `tzOffset` anchors buckets/heatmap to local time. */
  usageAnalytics: (days: number, tzOffset: number) =>
    api.get<UsageAnalytics>("/sessions/stats/usage", {
      days,
      tz_offset: tzOffset,
    }),
  // Full-transcript substring search. `includeArchived` sets scope
  // (live-only vs all); an empty `q` with `includeArchived` browses the
  // archive. Offset-paginated.
  searchSessions: (
    q: string,
    includeArchived: boolean,
    limit: number,
    offset: number,
  ) =>
    api.get<SessionListResponse>("/sessions/search", {
      q: q || undefined,
      include_archived: includeArchived || undefined,
      limit,
      offset,
    }),
  searchFieldValues: (field: string, q: string, context?: string) =>
    api.get<string[]>("/sessions/search/values", {
      field,
      q: q || undefined,
      context: context || undefined,
    }),
  session: (id: string) => api.get<SessionListItem>(`/sessions/${id}`),
  /** Mark this session's messages seen for the caller — clears its
   *  unread badge on the next `/sessions` refetch. */
  markSeen: (id: string) => api.post<void>(`/sessions/${id}/seen`),
  conversation: (
    id: string,
    opts?: {
      limit?: number;
      before?: number;
      after?: number;
      /** `asc` takes the OLDEST `limit` events; default `desc` (newest). The
       *  response is oldest-first either way. */
      order?: "asc" | "desc";
    },
  ) =>
    api.get<AgentEvent[]>(`/sessions/${id}/conversation`, {
      limit: opts?.limit,
      before: opts?.before,
      after: opts?.after,
      order: opts?.order,
    }),
  messagePins: (id: string) =>
    api.get<MessagePin[]>(`/sessions/${id}/pins`),
  pinMessage: (id: string, seq: number, messageId?: string | null) =>
    api.post<MessagePin>(`/sessions/${id}/pins`, {
      seq,
      message_id: messageId ?? null,
    }),
  unpinMessage: (id: string, seq: number) =>
    api.del<void>(`/sessions/${id}/pins/${seq}`),
  /** One-call session diagnose: everything the daemon knows about
   *  the session — each fact dated + sourced, plus the arbitration verdict —
   *  merged with the server-side gateway/account binding facts. */
  sessionDiagnose: (id: string) =>
    api.get<SessionDiagnoseResponse>(`/sessions/${id}/diagnose`),
  /** Per-session Langfuse cost/usage rollup, proxied server-side so
   *  the project keys never reach the browser. */
  sessionLangfuse: (id: string) =>
    api.get<LangfuseSessionUsage>(`/sessions/${id}/langfuse`),
  recentDirs: (machineId: string) =>
    api.get<string[]>("/sessions/recent-dirs", {
      machine_id: machineId || undefined,
    }),
  /** Sub-directories of `path` on a machine, for the working-dir
   *  autocomplete in the spawn dialog. */
  machineDirs: (machineId: string, path: string) =>
    api.get<{ dirs: string[] }>(`/machines/${machineId}/fs/dirs`, { path }),
  /** Branch / detached HEAD of `path` on a machine (spawn dialog badge). */
  machineGitInfo: (machineId: string, path: string) =>
    api.get<GitInfo>(`/machines/${machineId}/fs/gitinfo`, { path }),
  /** Machine/account-scoped codex model catalog. Empty `models`
   *  when none is cached yet — the picker falls back to its static list. */
  codexModels: (machineId: string) =>
    api.get<CodexModelCatalog>(`/machines/${machineId}/codex-models`),
  /** Codex catalog merged across every machine (newest report wins) for
   *  pickers with no machine in hand (dispatch, fork). */
  codexModelsMerged: () => api.get<CodexModelCatalog>("/models/codex"),
  /** Ask the machine's daemon to re-run codex `model/list`; the fresh
   *  catalog lands asynchronously. */
  refreshCodexModels: (machineId: string) =>
    api.post<void>(`/machines/${machineId}/codex-models/refresh`),
  users: () => api.get<UserRow[]>("/admin/users"),
  machines: (userId: string) =>
    api.get<MachineRow[]>(`/admin/users/${userId}/machines`),
  tokens: (userId: string) =>
    api.get<UserTokenRow[]>(`/admin/users/${userId}/tokens`),
  /** A user's scope ceiling. Self or admin. */
  userAcls: (userId: string) =>
    api.get<UserAclsResponse>(`/users/${userId}/acls`),
  /** A user's api_keys with their granted scopes. Self or admin. */
  userKeys: (userId: string) => api.get<ApiKeyRow[]>(`/users/${userId}/keys`),
  /** Spawn on a machine. Always `multipart/form-data`: the JSON
   *  `SpawnRequest` rides in the `request` part and any attached files ride as
   *  file parts the daemon stages under /tmp/cctui-uploads. */
  spawn: (body: SpawnRequest, files: File[] = []) => {
    const form = new FormData();
    form.append("request", JSON.stringify(body));
    for (const f of files) form.append("files", f, f.name);
    return api.postForm<SpawnResponse>("/sessions/spawn", form);
  },
  /** Stage mid-chat attachments for a running session. Same
   *  multipart shape + caps as spawn; resolves to the staged absolute paths
   *  on the session's machine, which the composer appends under the reply. */
  sessionAttachments: (sessionId: string) =>
    api.get<SessionAttachment[]>(`/sessions/${sessionId}/attachments`),
  stageFiles: (sessionId: string, files: File[]) => {
    const form = new FormData();
    for (const f of files) form.append("files", f, f.name);
    return api.postForm<StageFilesResponse>(
      `/sessions/${sessionId}/files`,
      form,
    );
  },
  /** Fork a conversation into a new session, optionally changing model/effort.
   *  Returns a `command_id` to await on the ws like spawn. */
  fork: (sessionId: string, body: ForkRequest) =>
    api.post<ForkResponse>(`/sessions/${sessionId}/fork`, body),
  resume: (sessionId: string) =>
    api.post<void>(`/sessions/${sessionId}/resume`, {}),
  /** Rebind one of a session's per-family gateway bindings.
   *  Pure server-side rebind: the worker keeps running and its next upstream
   *  call in the target credential's family lands on `account` (a name or id);
   *  the other family's binding is untouched. 409 when the session carries no
   *  binding in that family. */
  switchAccount: (sessionId: string, account: string) =>
    api.post<void>(`/sessions/${sessionId}/switch-account`, { account }),
  sessionBindings: (sessionId: string) =>
    api.get<SessionBinding[]>(`/sessions/${sessionId}/bindings`),
  /** Launch a draft session: env is entered fresh here (never stored
   *  in the draft), account gateway tokens minted server-side at dispatch. The
   *  draft row is removed and a live session is born from the daemon. */
  launchDraft: (sessionId: string, env: Record<string, string> = {}) =>
    api.post<SpawnResponse>(`/sessions/${sessionId}/launch`, { env }),
  /** Discard (delete) a draft session row. */
  discardDraft: (sessionId: string) =>
    api.post<void>(`/sessions/${sessionId}/discard`, {}),
  /** Replace a draft's stored spawn payload in place (edit / autosave). */
  updateDraft: (sessionId: string, body: SpawnRequest) =>
    api.put<SpawnResponse>(`/sessions/${sessionId}/draft`, body),
  dispatch: (body: DispatchRequest) =>
    api.post<DispatchResponse>("/sessions/dispatch", body),
  /** Configured dispatcher ids (e.g. `["claude-worker"]`); empty when none. */
  dispatchers: () => api.get<string[]>("/sessions/dispatchers"),
  /** The caller's enrolled dispatchers with liveness. */
  userDispatchers: () => api.get<UserDispatcher[]>("/dispatchers"),
  /** Enroll a dispatcher; the key is returned ONCE and never echoed again. */
  enrollDispatcher: (body: { name: string; kind?: string; account?: string; provider?: string }) =>
    api.post<EnrollDispatcherResponse>("/dispatcher/enroll", body),
  updateDispatcher: (id: string, body: RenameDispatcher) =>
    api.patch<UserDispatcher>(`/dispatchers/${id}`, body),
  deleteDispatcher: (id: string) => api.del<void>(`/dispatchers/${id}`),
  /** The caller's own OAuth accounts. Tokens never returned. */
  accounts: () => api.get<OAuthAccount[]>("/accounts"),
  /** Live launch-time redirect rules (all users' under the admin token). */
  redirects: () => api.get<AccountRedirect[]>("/redirects"),
  putRedirect: (accountId: string, body: PutRedirectRequest) =>
    api.put<AccountRedirect>(`/accounts/${accountId}/redirect`, body),
  deleteRedirect: (id: string) => api.del<void>(`/redirects/${id}`),
  /** The caller's account pools with their membership. */
  accountPools: () => api.get<AccountPoolView[]>("/account-pools"),
  /** Every pool's usage, aggregated per provider family (level, pace, wall). */
  accountPoolsUsage: () => api.get<PoolUsageView[]>("/account-pools/usage"),
  createAccountPool: (body: CreatePoolRequest) =>
    api.post<AccountPoolView>("/account-pools", body),
  updateAccountPool: (id: string, body: UpdatePoolRequest) =>
    api.patch<AccountPoolView>(`/account-pools/${id}`, body),
  deleteAccountPool: (id: string) => api.del<void>(`/account-pools/${id}`),
  /** Every mid-run account move this session made, newest first. */
  sessionRebinds: (id: string) =>
    api.get<SessionRebind[]>(`/sessions/${encodeURIComponent(id)}/rebinds`),
  /** The per-account settings catalog: exposable settings keys, the
   *  curated env allowlist, and the quiet-defaults preset — served from the
   *  server's embedded catalog so the editor can never drift from what the
   *  server validates on write. */
  settingsCatalog: (family = "anthropic") =>
    api.get<SettingsCatalogResponse>(
      `/accounts/settings-catalog?family=${encodeURIComponent(family)}`,
    ),
  /** Apply the current detector set to already-stored transcripts. `dry_run`
   *  reports counts and writes nothing; a real pass is irreversible. */
  rescrubSettings: (req: RescrubRequest) =>
    api.post<RescrubReport>("/settings/rescrub", req),
  createAccount: (body: CreateAccount) =>
    api.post<OAuthAccount>("/accounts", body),
  updateAccount: (id: string, body: UpdateAccount) =>
    api.patch<OAuthAccount>(`/accounts/${id}`, body),
  /** Edit one provider credential under an account. */
  updateProvider: (accountId: string, providerId: string, body: UpdateProvider) =>
    api.patch<AccountProvider>(`/accounts/${accountId}/providers/${providerId}`, body),
  /** Attach a provider credential to an existing account: the
   *  pasted-token / compatible-endpoint path. 409 on a family collision. */
  addProvider: (accountId: string, body: CreateProvider) =>
    api.post<AccountProvider>(`/accounts/${accountId}/providers`, body),
  /** Remove one provider credential; the identity + other providers stay. */
  deleteProvider: (accountId: string, providerId: string) =>
    api.del<void>(`/accounts/${accountId}/providers/${providerId}`),
  /** Re-parent a provider onto another account of the same owner. */
  moveProvider: (accountId: string, providerId: string, targetAccountId: string) =>
    api.post<AccountProvider>(`/accounts/${accountId}/providers/${providerId}/move`, {
      target_account_id: targetAccountId,
    }),
  deleteAccount: (id: string) => api.del<void>(`/accounts/${id}`),
  /** Current subscription usage for an account. Free + tokenless;
   *  the server slow-refreshes a cache so polling never spams upstream. */
  accountUsage: (id: string) => api.get<AccountUsage>(`/accounts/${id}/usage`),
  accountLimitReset: (id: string, creditId?: string | null) =>
    api.post<LimitResetResponse>(`/accounts/${id}/limit-reset`, { credit_id: creditId ?? null }),
  /** Who an account is shared with. Owner-scoped server-side. */
  accountShares: (id: string) => api.get<ShareInfo[]>(`/accounts/${id}/shares`),
  grantShare: (id: string, body: GrantShare) =>
    api.post<ShareInfo>(`/accounts/${id}/shares`, body),
  revokeShare: (id: string, userId: string) =>
    api.del<void>(`/accounts/${id}/shares/${userId}`),
  /** Generic resource sharing. `resourceType` is the DB kind
   *  (`account` | `machine` | `dispatcher` | `context_pack`); owner-scoped. */
  resourceShares: (resourceType: string, id: string) =>
    api.get<ResourceShareInfo[]>(`/${resourceType}/${id}/shares`),
  grantResourceShare: (resourceType: string, id: string, body: GrantShare) =>
    api.post<ResourceShareInfo>(`/${resourceType}/${id}/shares`, body),
  revokeResourceShare: (resourceType: string, id: string, userId: string) =>
    api.del<void>(`/${resourceType}/${id}/shares/${userId}`),
  oauthStart: (provider: string, userId?: string, accountId?: string) =>
    api.post<OAuthStartResponse>("/accounts/oauth/start", {
      provider,
      user_id: userId,
      // Attach target: finish lands the credential as a provider
      // under this existing account instead of creating a new identity.
      account_id: accountId,
    }),
  oauthFinish: (body: OAuthFinish) =>
    api.post<OAuthAccount>("/accounts/oauth/finish", body),
  /** The caller's daemon machines (every user's for an admin) with their
   *  last host CPU / memory / disk snapshot — Settings › Resource monitoring
   *  and the header gauge. */
  machineResources: () =>
    api.get<MachineResourcesRow[]>("/machines/resources"),
  /** RAM ceiling for spawns on one machine; `null` removes it. Spawns over
   *  it wait in a queue (`queued` sessions) until memory frees up. */
  setMemCeiling: (machineId: string, bytes: number | null) =>
    api.put<void>(`/machines/${machineId}/mem-ceiling`, {
      mem_ceiling_bytes: bytes,
    } satisfies MemCeilingRequest),
  /** Every spawnable machine across all active users — for the spawn picker.
   * Excludes server-managed machines (`ephemeral` worker pods and the per-user
   * `dispatch` machine): those aren't somewhere you'd start an interactive
   * session, only real enrolled daemons are. */
  allMachines: async (): Promise<MachineRow[]> => {
    const users = (await api.get<UserRow[]>("/admin/users")).filter(
      (u) => !u.revoked_at,
    );
    const lists = await Promise.all(
      users.map((u) => api.get<MachineRow[]>(`/admin/users/${u.id}/machines`)),
    );
    return lists
      .flat()
      .filter((m) => !m.revoked_at && !SYSTEM_MACHINE_KINDS.has(m.kind));
  },
};
