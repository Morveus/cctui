import { useQueryClient } from "@tanstack/svelte-query";
import { api } from "../api";
import { endpoints } from "./endpoints";
import { qk } from "./keys";
import type { SessionListItem } from "@bindings/SessionListItem";
import type { SessionListResponse } from "@bindings/SessionListResponse";
import type { DispatchRequest } from "@bindings/DispatchRequest";
import type { SpawnRequest } from "@bindings/SpawnRequest";
import type { SpawnResponse } from "@bindings/SpawnResponse";
import type { ForkRequest } from "@bindings/ForkRequest";
import type { Label } from "@bindings/Label";
import type { KeepaliveState } from "@bindings/KeepaliveState";
import type { SessionKeepaliveRequest } from "@bindings/SessionKeepaliveRequest";

/** Build a placeholder card for an in-flight dispatch. Mirrors the
 * fields the worker will report once its daemon registers, so the optimistic
 * card looks like the real one until the refetch reconciles it by id. */
function optimisticDispatchCard(
  id: string,
  body: DispatchRequest,
): SessionListItem {
  const p = (body.payload ?? {}) as Record<string, string>;
  return {
    id,
    parent_id: null,
    machine_id: "dispatch",
    // Real cwd is unknown until the worker registers; show the target repo if
    // the payload carries one, else nothing (no `dispatch:<origin>` noise).
    working_dir: p.repo ?? "",
    status: "new",
    liveness: "stale",
    attention: null,
    bucket: "working",
    token_usage: {
      tokens_in: 0,
      tokens_out: 0,
      cost_usd: 0,
      cache_read_tokens: 0,
      cache_creation_tokens: 0,
    },
    metadata: null,
    adapter_id: "claude-code",
    machine_name: "dispatch",
    machine_hue: null,
    // Lands the optimistic card straight in the Dispatched group.
    machine_kind: "dispatch",
    last_message_text: "Dispatching…",
    last_message_at: null,
    registered_at: null,
    name:
      p.name ||
      p.prompt_file ||
      (p.prompt ? p.prompt.slice(0, 40) : null) ||
      id.slice(0, 6),
    model: p.model ?? null,
    effort: p.effort ?? null,
    auto_approve: false,
    match_snippet: null,
    last_activity_at: null,
    cache_cold: false,
    estimated_burst_tokens: null,
    hibernated: false,
    pinned: false,
    labels: [],
    last_heartbeat: null,
    pr_links: [],
    account_name: body.account ?? null,
    unread_count: 0,
    activity_detail: null,
    last_tool_at: null,
    last_tool_name: null,
    tool_use_count: 0,
    todos: [],
    has_token_credentials: false,
    account_traffic_observed: false,
  };
}

export function useSessionActions() {
  const qc = useQueryClient();
  const inval = () => qc.invalidateQueries({ queryKey: ["sessions"] });
  const invalLabels = () => qc.invalidateQueries({ queryKey: qk.labels });
  return {
    rename: async (id: string, name: string) => {
      await api.patch<void>(`/sessions/${id}`, { name });
      inval();
    },
    // Mark a session's messages seen. The caller invalidates the list
    // itself once the seen-mark lands, so this doesn't refetch on its own.
    markSeen: async (id: string) => {
      await endpoints.markSeen(id);
    },
    // Single-row archive is a deliberate gesture on that one session, so it
    // may archive a pinned session; batches and automations never can.
    archive: async (id: string) => {
      await api.post<void>(`/sessions/${id}/archive?force=true`);
      inval();
    },
    unarchive: async (id: string) => {
      await api.post<void>(`/sessions/${id}/unarchive`);
      inval();
    },
    // Pin/unpin: pinned sessions sort to the top and are exempt
    // from auto-archive. Pinning an archived session also un-archives it.
    pin: async (id: string) => {
      await api.post<void>(`/sessions/${id}/pin`);
      inval();
    },
    unpin: async (id: string) => {
      await api.post<void>(`/sessions/${id}/unpin`);
      inval();
    },
    setKeepalive: async (
      id: string,
      body: SessionKeepaliveRequest,
    ): Promise<KeepaliveState | null> => {
      const state = await api.post<KeepaliveState | null>(
        `/sessions/${id}/keepalive`,
        body,
      );
      inval();
      return state;
    },
    // Labels. `createLabel` is get-or-create by name (and recolors an
    // existing one); attach/detach wire a label to a session. Each mutation
    // refreshes the session list so the chips update in place.
    createLabel: async (name: string, color: string): Promise<Label> => {
      const label = await api.post<Label>("/labels", { name, color });
      invalLabels();
      return label;
    },
    // Edit a specific label in place (rename and/or recolor) — keyed on id, so
    // unlike `createLabel` it can rename without orphaning the old name.
    updateLabel: async (
      labelId: string,
      patch: { name?: string; color?: string },
    ): Promise<Label> => {
      const label = await api.patch<Label>(`/labels/${labelId}`, patch);
      invalLabels();
      inval();
      return label;
    },
    deleteLabel: async (labelId: string) => {
      await api.del<void>(`/labels/${labelId}`);
      invalLabels();
      inval();
    },
    attachLabel: async (id: string, labelId: string) => {
      await api.post<void>(`/sessions/${id}/labels`, { label_id: labelId });
      inval();
    },
    detachLabel: async (id: string, labelId: string) => {
      await api.del<void>(`/sessions/${id}/labels/${labelId}`);
      inval();
    },
    // Batch archive/unarchive. One request, one invalidation.
    archiveMany: async (ids: string[]) => {
      if (ids.length === 0) return;
      await api.post<void>("/sessions/archive", { ids });
      inval();
    },
    unarchiveMany: async (ids: string[]) => {
      if (ids.length === 0) return;
      await api.post<void>("/sessions/unarchive", { ids });
      inval();
    },
    kill: async (id: string) => {
      await api.post<void>(`/sessions/${id}/kill`);
      inval();
    },
    /** Stop the in-flight turn. Returns a `command_id` to await on
     *  the ws so the caller can tell whether the agent actually
     *  accepted the interrupt instead of firing-and-forgetting. */
    interrupt: async (id: string) =>
      api.post<SpawnResponse>(`/sessions/${id}/interrupt`),
    // In-place model/effort switch. Codex carries it on the next
    // turn/start and echoes the resolved values back via Status; claude rejects
    // it (the UI offers fork-to-change-model for claude instead). Returns a
    // `command_id` to await on the ws so the caller confirms the
    // change only once the adapter truthfully applied it.
    setModel: async (id: string, model?: string, effort?: string) => {
      const res = await api.post<SpawnResponse>(`/sessions/${id}/set-model`, {
        model: model || null,
        effort: effort || null,
      });
      inval();
      return res;
    },
    setAutoApprove: async (id: string, enabled: boolean) => {
      await api.post<void>(`/sessions/${id}/auto-approve`, { enabled });
      inval();
    },
    spawn: (body: SpawnRequest, files: File[] = []) =>
      endpoints.spawn(body, files),
    // Draft sessions: launch promotes a draft to a live spawn (env
    // entered fresh), discard deletes it. Both refetch the roster.
    launchDraft: async (id: string, env: Record<string, string> = {}) => {
      const res = await endpoints.launchDraft(id, env);
      inval();
      return res;
    },
    discardDraft: async (id: string) => {
      await endpoints.discardDraft(id);
      inval();
    },
    updateDraft: async (id: string, body: SpawnRequest) => {
      const res = await endpoints.updateDraft(id, body);
      inval();
      return res;
    },
    // Fork a conversation into a new session. Optionally overrides
    // model/effort (the "fork to change model" path for claude). The new
    // session links back to the parent and registers shortly after; refetch.
    fork: async (id: string, body: ForkRequest) => {
      const res = await endpoints.fork(id, body);
      inval();
      return res;
    },
    resume: async (id: string) => {
      await endpoints.resume(id);
      inval();
    },
    // Mid-chat attachments: stage files for a running session and
    // return the staged paths the composer references under the reply.
    // The recorded names are what keeps the next paste off a colliding
    // `paste-N.txt`, so they must not stay stale for the query's staleTime.
    stageFiles: async (id: string, files: File[]) => {
      const res = await endpoints.stageFiles(id, files);
      await qc.invalidateQueries({ queryKey: qk.sessionAttachments(id) });
      return res;
    },
    // Dispatch returns synchronously (no daemon ACK / command_id), so unlike
    // spawn there's nothing to await on the ws — the worker pod registers the
    // pre-minted session_id later. We optimistically insert a placeholder card
    // keyed by the client-minted session_id so the list updates IMMEDIATELY
    //; the eventual refetch reconciles it by id (the worker pod, or
    // the server's `failed` row on a backend error, both carry the same id).
    dispatch: async (body: DispatchRequest) => {
      const key = qk.sessions(false);
      const id = body.session_id ?? crypto.randomUUID();
      if (body.session_id == null) body = { ...body, session_id: id };
      const placeholder = optimisticDispatchCard(id, body);
      qc.setQueryData<SessionListResponse>(key, (prev) => ({
        sessions: [
          placeholder,
          ...(prev?.sessions ?? []).filter((s) => s.id !== id),
        ],
      }));
      try {
        const res = await endpoints.dispatch(body);
        inval();
        return res;
      } catch (e) {
        // Reconcile to server truth (the row exists as `failed`); the card
        // stays visible so the user can see + retry the failed dispatch.
        inval();
        throw e;
      }
    },
  };
}
