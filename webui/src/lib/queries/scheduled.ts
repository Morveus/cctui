import { createQuery, useQueryClient } from "@tanstack/svelte-query";
import type { SpawnResponse } from "@bindings/SpawnResponse";
import { api } from "../api";

export type ScheduledState = "scheduled" | "sending" | "sent" | "cancelled" | "dead";

export interface ScheduledMessage {
  id: string;
  body: string;
  state: ScheduledState;
  deliver_at: string;
  attempts: number;
  last_error: string | null;
  turn_id: string;
  origin: "human" | "macro";
  created_at: string;
  sent_at: string | null;
}

export const scheduledKey = (sessionId: string) => ["scheduled-messages", sessionId] as const;

const base = (sessionId: string) =>
  `/sessions/${encodeURIComponent(sessionId)}/messages/scheduled`;

export const scheduledEndpoints = {
  list: (sessionId: string) => api.get<ScheduledMessage[]>(base(sessionId)),
  schedule: (sessionId: string, content: string, deliverAt: Date) =>
    api.post<SpawnResponse>(`/sessions/${encodeURIComponent(sessionId)}/message`, {
      content,
      deliver_at: deliverAt.toISOString(),
    }),
  update: (sessionId: string, id: string, patch: { body?: string; deliver_at?: string }) =>
    api.patch<void>(`${base(sessionId)}/${id}`, patch),
  cancel: (sessionId: string, id: string) => api.del<void>(`${base(sessionId)}/${id}`),
  sendNow: (sessionId: string, id: string) => api.post<void>(`${base(sessionId)}/${id}/send-now`),
};

export const pendingScheduled = (rows: ScheduledMessage[] | undefined) =>
  (rows ?? []).filter((r) => r.state === "scheduled" || r.state === "sending" || r.state === "dead");

/** turn_id → deliver_at for delivered scheduled messages. */
export const scheduledTurns = (rows: ScheduledMessage[] | undefined) =>
  new Map((rows ?? []).filter((r) => r.state === "sent").map((r) => [r.turn_id, r.deliver_at]));

export const useScheduledMessages = (sessionId: () => string) =>
  createQuery(() => ({
    queryKey: scheduledKey(sessionId()),
    queryFn: () => scheduledEndpoints.list(sessionId()),
    refetchInterval: 30_000,
  }));

export const useScheduledActions = (sessionId: () => string) => {
  const qc = useQueryClient();
  const run = async <T>(p: Promise<T>) => {
    try {
      return await p;
    } finally {
      void qc.invalidateQueries({ queryKey: scheduledKey(sessionId()) });
    }
  };
  return {
    schedule: (content: string, at: Date) =>
      run(scheduledEndpoints.schedule(sessionId(), content, at)),
    update: (id: string, patch: { body?: string; deliver_at?: string }) =>
      run(scheduledEndpoints.update(sessionId(), id, patch)),
    cancel: (id: string) => run(scheduledEndpoints.cancel(sessionId(), id)),
    sendNow: (id: string) => run(scheduledEndpoints.sendNow(sessionId(), id)),
  };
};
