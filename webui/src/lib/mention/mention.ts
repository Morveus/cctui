// Pure logic for the `#` session-mention popover shared by the chat composer
// and the spawn prompt fields. Typing `#` opens a list of non-archived
// sessions (completed ones included); picking one
// replaces the `#query` under the caret with `#<id> (<name>) ` so the user can
// hand the id to their agent ("sync with #<id>").
import type { SessionListItem } from "@bindings/SessionListItem";

export type MentionTrigger = {
  /** Index of the `#` character in the text. */
  start: number;
  /** Text typed after the `#`, up to the caret (never contains whitespace). */
  query: string;
};

/**
 * Find an active `#query` immediately before the caret. Returns null when the
 * caret is not inside one: no `#`, whitespace between `#` and the caret (so
 * "ticket #12 " stays a plain hash), or the `#` glued to a preceding word
 * character (e.g. `C#`, a URL fragment).
 */
export function findTrigger(
  text: string,
  caret: number,
): MentionTrigger | null {
  const before = text.slice(0, caret);
  const hash = before.lastIndexOf("#");
  if (hash < 0) return null;
  const query = before.slice(hash + 1);
  if (/\s/.test(query)) return null;
  if (hash > 0 && /[\w#]/.test(before[hash - 1])) return null;
  return { start: hash, query };
}

/** Every session an agent could still be pointed at: any bucket, completed
 *  ones included, but never archived or draft. `excludeId` drops the session
 *  the composer belongs to. */
export function mentionableSessions(
  sessions: SessionListItem[],
  excludeId?: string | null,
): SessionListItem[] {
  return sessions.filter(
    (s) =>
      s.id !== excludeId &&
      s.status !== "archived" &&
      s.status !== "draft" &&
      s.status !== "queued",
  );
}

/** Case-insensitive match on name, id, working dir and machine name. */
export function filterMentions(
  sessions: SessionListItem[],
  query: string,
): SessionListItem[] {
  const q = query.trim().toLowerCase();
  if (!q) return sessions;
  return sessions.filter((s) =>
    [s.name, s.id, s.working_dir, s.machine_name].some((f) =>
      f?.toLowerCase().includes(q),
    ),
  );
}

/** The inserted token: `#<id> (<name>)`, or just `#<id>` for unnamed sessions. */
export function mentionToken(s: Pick<SessionListItem, "id" | "name">): string {
  const name = s.name?.trim();
  return name ? `#${s.id} (${name})` : `#${s.id}`;
}

/** Replace the `#query` under the caret with the mention token plus a trailing
 *  space; returns the new text and where the caret should land. */
export function applyMention(
  text: string,
  caret: number,
  trigger: MentionTrigger,
  s: Pick<SessionListItem, "id" | "name">,
): { text: string; caret: number } {
  const token = mentionToken(s) + " ";
  const next = text.slice(0, trigger.start) + token + text.slice(caret);
  return { text: next, caret: trigger.start + token.length };
}

/** Wrap-around move of the highlighted row. */
export function moveSelection(
  index: number,
  delta: 1 | -1,
  length: number,
): number {
  if (length === 0) return 0;
  return (index + delta + length) % length;
}
