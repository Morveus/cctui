export interface LabelAttacher {
  attachLabel: (sessionId: string, labelId: string) => Promise<unknown>;
}

/** Attach labels to a known session; a deleted label or a blip never fails
 *  the spawn that carried it. Machine spawns and draft launches don't need
 *  this: they send `label_ids` and the server attaches them on registration. */
export async function attachLabelsTo(
  api: LabelAttacher,
  sessionId: string,
  ids: string[],
) {
  for (const id of ids) {
    try {
      await api.attachLabel(sessionId, id);
    } catch {
      /* best-effort */
    }
  }
}
