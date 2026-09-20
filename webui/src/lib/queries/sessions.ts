import { createQuery, useQueryClient } from "@tanstack/svelte-query";
import type { AgentEvent } from "@bindings/AgentEvent";
import type { MessagePin } from "@bindings/MessagePin";
import { endpoints } from "./endpoints";
import { qk } from "./keys";

export const useSessions = (
  archived: () => boolean,
  enabled: () => boolean = () => true,
) =>
  createQuery(() => ({
    queryKey: qk.sessions(archived()),
    queryFn: () => endpoints.sessions(archived()),
    refetchInterval: 15_000,
    enabled: enabled(),
  }));

export const useSessionStats = () =>
  createQuery(() => ({
    queryKey: qk.sessionStats,
    queryFn: () => endpoints.sessionStats(Intl.DateTimeFormat().resolvedOptions().timeZone),
    refetchInterval: 15_000,
  }));

/** All label definitions. Shared by the per-session picker and the
 * sessions-page filter; refetched lazily since labels change rarely. */
export const useLabels = () =>
  createQuery(() => ({
    queryKey: qk.labels,
    queryFn: endpoints.labels,
    refetchInterval: 60_000,
  }));

export const useTokenStats = () =>
  createQuery(() => ({
    queryKey: qk.tokenStats,
    // Resolve the offset per fetch so it stays correct across a DST change.
    queryFn: () => endpoints.tokenStats(new Date().getTimezoneOffset()),
    refetchInterval: 15_000,
  }));

export const useUsageAnalytics = (days: () => number) =>
  createQuery(() => ({
    queryKey: qk.usageAnalytics(days()),
    queryFn: () =>
      endpoints.usageAnalytics(days(), new Date().getTimezoneOffset()),
    refetchInterval: 60_000,
  }));

/** Older pages (`before` cursor) deliberately bypass the query cache. */
export const CONVERSATION_FETCH_LIMIT = 60;

/** Must outlive the persister's maxAge or restored entries get GC'd. */
export const CONVERSATION_GC_MS = 24 * 60 * 60 * 1000;

export const useConversation = (
  id: () => string,
  enabled: () => boolean = () => true,
) => {
  const qc = useQueryClient();
  return createQuery(() => ({
    queryKey: qk.conversation(id()),
    // Empty delta must return `prev` itself: a new array identity would
    // re-render the whole drawer for nothing.
    queryFn: async () => {
      const prev = qc.getQueryData<AgentEvent[]>(qk.conversation(id()));
      const lastSeq = (prev ?? []).reduce((m, e) => Math.max(m, e.seq ?? 0), 0);
      if (prev?.length && lastSeq > 0) {
        const delta = await endpoints.conversation(id(), { after: lastSeq });
        return delta.length ? [...prev, ...delta] : prev;
      }
      return endpoints.conversation(id(), { limit: CONVERSATION_FETCH_LIMIT });
    },
    enabled: enabled() && !!id(),
    // Reopening the drawer must always issue the `after=lastSeq` delta, even
    // inside the global 5s staleTime: the ws is unsubscribed while the drawer
    // is closed, so live merging cannot have covered that window. The delta is
    // bounded and returns `prev` by identity when empty.
    refetchOnMount: "always",
    gcTime: CONVERSATION_GC_MS,
  }));
};

/** Oldest events of a session, independent of the tail window the drawer
 *  renders — the brief strip needs the FIRST user message, which paging back
 *  from the tail would take many fetches to reach. */
export const CONVERSATION_HEAD_LIMIT = 20;

export const useConversationHead = (
  id: () => string,
  enabled: () => boolean = () => true,
) =>
  createQuery(() => ({
    queryKey: qk.conversationHead(id()),
    queryFn: () =>
      endpoints.conversation(id(), {
        limit: CONVERSATION_HEAD_LIMIT,
        order: "asc",
      }),
    enabled: enabled() && !!id(),
    // The head of a transcript never changes.
    staleTime: Infinity,
    gcTime: CONVERSATION_GC_MS,
  }));

/** Messages the caller pinned in this session (server-side, per user). */
export const useMessagePins = (
  id: () => string,
  enabled: () => boolean = () => true,
) =>
  createQuery(() => ({
    queryKey: qk.messagePins(id()),
    queryFn: () => endpoints.messagePins(id()),
    enabled: enabled() && !!id(),
    staleTime: 30_000,
  }));

/** Optimistic pin/unpin: the glyph and the panel must flip on click, not a
 *  round-trip later. */
export function useMessagePinActions() {
  const qc = useQueryClient();
  const key = (id: string) => qk.messagePins(id);
  const write = (id: string, next: MessagePin[]) =>
    qc.setQueryData<MessagePin[]>(key(id), next);
  const read = (id: string) => qc.getQueryData<MessagePin[]>(key(id)) ?? [];
  const inval = (id: string) => qc.invalidateQueries({ queryKey: key(id) });
  return {
    async pin(id: string, seq: number, messageId?: string | null) {
      const prev = read(id);
      if (!prev.some((p) => p.seq === seq))
        write(id, [
          ...prev,
          {
            session_id: id,
            seq,
            message_id: messageId ?? null,
            note: null,
            created_at: new Date().toISOString(),
          },
        ].sort((a, b) => a.seq - b.seq));
      try {
        await endpoints.pinMessage(id, seq, messageId);
      } catch (e) {
        write(id, prev);
        throw e;
      } finally {
        void inval(id);
      }
    },
    async unpin(id: string, seq: number) {
      const prev = read(id);
      write(id, prev.filter((p) => p.seq !== seq));
      try {
        await endpoints.unpinMessage(id, seq);
      } catch (e) {
        write(id, prev);
        throw e;
      } finally {
        void inval(id);
      }
    },
  };
}

/** Session diagnose panel. Fetched only while the panel is open;
 *  no polling — the panel offers an explicit refresh instead, since the call
 *  round-trips through the daemon. */
export const useSessionDiagnose = (
  id: () => string,
  enabled: () => boolean = () => true,
) =>
  createQuery(() => ({
    queryKey: ["session-diagnose", id()],
    queryFn: () => endpoints.sessionDiagnose(id()),
    enabled: enabled() && !!id(),
    staleTime: 0,
    retry: false,
  }));

/** Per-session Langfuse cost/usage chip. Lazy — fetched only while
 *  the drawer is open and the capability is present; the server caches ~60s so
 *  a short client stale time won't hammer upstream. Fail-open: on error the
 *  chip simply hides. */
export const useSessionLangfuse = (
  id: () => string,
  enabled: () => boolean = () => true,
) =>
  createQuery(() => ({
    queryKey: ["session-langfuse", id()],
    queryFn: () => endpoints.sessionLangfuse(id()),
    enabled: enabled() && !!id(),
    staleTime: 60_000,
    retry: false,
  }));

/** Files the user uploaded into a session. Immutable per session id (a
 *  send appends, never rewrites), so the sender invalidates after staging. */
export const useSessionAttachments = (
  id: () => string,
  enabled: () => boolean = () => true,
) =>
  createQuery(() => ({
    queryKey: qk.sessionAttachments(id()),
    queryFn: () => endpoints.sessionAttachments(id()),
    enabled: enabled() && !!id(),
    staleTime: 5 * 60_000,
    retry: false,
  }));

export const useRecentDirs = (machineId: () => string) =>
  createQuery(() => ({
    queryKey: ["recent-dirs", machineId()],
    queryFn: () => endpoints.recentDirs(machineId()),
    enabled: !!machineId(),
    staleTime: 30_000,
  }));

export const useMachineDirs = (machineId: () => string, path: () => string) =>
  createQuery(() => ({
    queryKey: ["machine-dirs", machineId(), path()],
    queryFn: () => endpoints.machineDirs(machineId(), path()),
    enabled: !!machineId() && !!path(),
    staleTime: 10_000,
    retry: false,
  }));

export const useCodexModels = (machineId: () => string) =>
  createQuery(() => ({
    queryKey: ["codex-models", machineId()],
    queryFn: () => endpoints.codexModels(machineId()),
    enabled: !!machineId(),
    staleTime: 60_000,
    retry: false,
  }));

export const useMergedCodexModels = (enabled: () => boolean = () => true) =>
  createQuery(() => ({
    queryKey: ["codex-models", "merged"],
    queryFn: () => endpoints.codexModelsMerged(),
    enabled: enabled(),
    staleTime: 60_000,
    retry: false,
  }));

export const useSessionBindings = (sessionId: () => string, enabled: () => boolean = () => true) =>
  createQuery(() => ({
    queryKey: ["session-bindings", sessionId()],
    queryFn: () => endpoints.sessionBindings(sessionId()),
    enabled: enabled(),
  }));
