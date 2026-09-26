// Pure formatting / parsing / dedup helpers for the conversation drawer,
// extracted from ConversationDrawer.svelte (no behavior change). Everything
// here is side-effect free — view-dependent formatting takes its toggles as
// explicit args so these stay testable and decoupled from component state.
import type { AgentEvent } from '@bindings/AgentEvent';
import { userMsgKey } from '$lib/ws.svelte';
import { prettyJson } from '$lib/markdown';
import { m } from '$lib/paraglide/messages';
import type { AskQuestion, Line, TodoItem, TodoProgress, TodoStatus } from './types';

// Some "user" turns are really harness/system messages directed at the agent
// (timer wake-ups, task-completion notifications, injected reminders, skill
// preambles, hook feedback) rather than something the human typed. We classify
// these structurally, by the fixed marker the harness/Claude prefixes them with.
//
// We deliberately ignore the stored `meta` bit (Claude's `isMeta`): cctui
// delivers a human's composer reply through Claude's control-socket `reply` op,
// which Claude records `isMeta:true`, so trusting it reclassified genuine human
// turns to `system` and made them appear to vanish on reload. Keep
// this list in sync with `META_MARKERS` in the daemon's transcript parser.
export const META_TAGS = [
	'<task-notification',
	'<system-reminder',
	'<command-name',
	'<command-message',
	'<local-command',
	'<bash-input',
	'<bash-stdout',
	'<bash-stderr',
	'[SYSTEM NOTIFICATION',
	'Base directory for this skill:',
	'Stop hook feedback:',
	'# Autonomous loop'
];
// Markers are matched at the start of ANY line, not only the start of the turn:
// the harness routinely prefixes its own sentence before the wrapper it injects,
// which a prefix-only test never sees. Line-anchored rather than a bare
// substring scan so a human quoting `<system-reminder>` inside a sentence stays
// a human turn. Mirrors `user_text_is_meta` in the daemon's transcript parser.
export function looksMeta(text: string): boolean {
	return (
		isSyntheticImageNotice(text) ||
		text.split('\n').some((line) => {
			const t = line.trimStart();
			return META_TAGS.some((m) => t.startsWith(m));
		})
	);
}

// The harness wraps a peer agent's message in this tag but prefixes its own
// "Another Claude session sent a message:" line, so the tag is never at the
// start of the turn — the preamble is what identifies the shape.
export const PEER_PREAMBLES = [
	'Another Claude session sent a message:',
	'Another session sent a message:',
	'Received a message from agent'
];
// Line-anchored so a human quoting or relaying a wrapper stays a human turn.
const PEER_TAG_RE = /^<(cross-session-message|agent-message)\b([^>]*)>([\s\S]*?)<\/\1>/m;
const PEER_ATTR_RE = /([a-z-]+)="([^"]*)"/g;

export interface PeerMessage {
	/** `from-name` when the sender supplied one, else the raw `from` address. */
	from: string | null;
	body: string;
}

function isPeerPreamble(line: string): boolean {
	const t = line.trim();
	return PEER_PREAMBLES.some((p) => t.startsWith(p));
}

export function parsePeerMessage(text: string): PeerMessage | null {
	const tag = PEER_TAG_RE.exec(text);
	if (!tag) return null;
	const before = text.slice(0, tag.index);
	if (before.split('\n').some((l) => l.trim() && !isPeerPreamble(l))) return null;
	const attrs = new Map<string, string>();
	for (const a of tag[2].matchAll(PEER_ATTR_RE)) attrs.set(a[1], a[2]);
	const name = attrs.get('from-name')?.trim();
	const addr = attrs.get('from')?.trim();
	return { from: name || addr || null, body: tag[3].trim() };
}

// Claude stores an attachment-carrying user turn as three separate
// `stream_events` rows in three different encodings (composer prose + staged
// paths, Claude's `[Image #N][name]`-prefixed copy, a synthetic `[Image: …]`
// line), so no content hash collapses them. Reducing all three to the human
// prose is what makes one turn render as one bubble.
const ATTACHED_HEADER_RE = /^Attached files?(\s*\(\d+\))?:$/i;
const STAGED_BULLET_RE = /^-\s*\S*\/?cctui-uploads\/\S+/;
// The wording changes between Claude releases (`source: …`, `original WxH,
// displayed at …`, `#2`), so match the family by bracket shape, not a literal.
const SYNTH_IMAGE_LINE_RE = /^\[Image(?:[:#][^\]]*|\s[^\]]*)?\]$/;
export const IMAGE_TOKEN_RUN_RE = /^(?:\s*\[(?:Image #\d+|[^[\]\n]*\.[A-Za-z0-9]{1,8})\])+\s*/;

export function isSyntheticImageNotice(text: string): boolean {
	const lines = text
		.split('\n')
		.map((l) => l.trim())
		.filter(Boolean);
	return lines.length > 0 && lines.every((l) => SYNTH_IMAGE_LINE_RE.test(l));
}

export function stripAttachmentDecorations(text: string): string {
	const out: string[] = [];
	let inAttachedBlock = false;
	let first = true;
	for (const raw of text.split('\n')) {
		const line = raw.trimEnd();
		const t = line.trim();
		if (ATTACHED_HEADER_RE.test(t)) {
			inAttachedBlock = true;
			continue;
		}
		if (inAttachedBlock) {
			if (!t || t.startsWith('-')) continue;
			inAttachedBlock = false;
		}
		if (SYNTH_IMAGE_LINE_RE.test(t) || STAGED_BULLET_RE.test(t)) continue;
		// The token run is only ever prefixed to the turn's opening line; a later
		// `[file.txt]` is the human's own prose and must survive.
		out.push(first ? line.replace(IMAGE_TOKEN_RUN_RE, '') : line);
		if (t) first = false;
	}
	return out.join('\n').trim();
}

// Pull a well-formed questions[] out of an AskUserQuestion tool input.
export function parseAsk(input: unknown): AskQuestion[] | null {
	const qs = (input as { questions?: unknown })?.questions;
	if (!Array.isArray(qs) || qs.length === 0) return null;
	const out = qs
		.filter(
			(q): q is AskQuestion =>
				!!q && typeof (q as AskQuestion).question === 'string' && Array.isArray((q as AskQuestion).options)
		)
		.map((q) => ({
			header: q.header,
			question: q.question,
			multiSelect: !!q.multiSelect,
			options: q.options.map((o) => ({ label: String(o.label ?? ''), description: o.description, preview: o.preview }))
		}));
	return out.length ? out : null;
}

// Pull the plan markdown out of an ExitPlanMode tool input. The
// peer of `parseAsk` — used to render a historic plan tool_call as a Plan card.
export function parsePlan(input: unknown): string | null {
	const plan = (input as { plan?: unknown })?.plan;
	if (typeof plan !== 'string' || plan.trim().length === 0) return null;
	return plan;
}

function blockedBy(raw: unknown): string[] | undefined {
	if (!Array.isArray(raw)) return undefined;
	const out = raw.filter((v): v is string => typeof v === 'string' && v.trim().length > 0);
	return out.length ? out : undefined;
}

function todoStatus(raw: unknown): TodoStatus {
	switch (raw) {
		case 'in_progress':
		case 'completed':
			return raw;
		default:
			return 'pending';
	}
}

// Null for malformed/empty input so the caller degrades to the generic JSON
// bubble rather than rendering an empty card.
export function parseTodos(input: unknown): TodoItem[] | null {
	const src = input as { todos?: unknown; plan?: unknown } | null | undefined;
	const raw = Array.isArray(src?.todos) ? src.todos : Array.isArray(src?.plan) ? src.plan : null;
	if (!raw || raw.length === 0) return null;
	const out: TodoItem[] = [];
	for (const e of raw) {
		if (!e || typeof e !== 'object') continue;
		const r = e as {
			content?: unknown;
			step?: unknown;
			status?: unknown;
			activeForm?: unknown;
			blockedBy?: unknown;
			blocked_by?: unknown;
		};
		const content = typeof r.content === 'string' ? r.content : typeof r.step === 'string' ? r.step : '';
		if (!content.trim()) continue;
		out.push({
			content,
			status: todoStatus(r.status),
			activeForm: typeof r.activeForm === 'string' && r.activeForm.trim() ? r.activeForm : undefined,
			blockedBy: blockedBy(r.blockedBy ?? r.blocked_by)
		});
	}
	return out.length ? out : null;
}

export function todoProgress(items: TodoItem[] | null | undefined): TodoProgress | null {
	if (!items?.length) return null;
	return {
		items,
		done: items.filter((t) => t.status === 'completed').length,
		total: items.length,
		inProgress: items.find((t) => t.status === 'in_progress') ?? null
	};
}

// The consecutive-duplicate guard keys on `text`/`html`, which a todo line does
// not carry: without this discriminator successive TodoWrites collapse into the
// FIRST one and the drawer renders the stalest list forever.
export function todoSignature(items: TodoItem[]): string {
	return items.map((t) => `${t.status}:${t.content}`).join('|');
}

// Key of the only task-list line that may render a card. Must be computed over
// the FULL transcript, never the render window, or paging older lines in would
// promote a stale list to "newest".
export function latestTodoLineKey(lines: Line[]): string | undefined {
	for (let i = lines.length - 1; i >= 0; i--) if (lines[i].todos) return lines[i].key;
	return undefined;
}

// Content signature of an event, used to dedup the live stream against fetched
// history (the same logical event has a DIFFERENT `ts` in each source — history
// stamps DB `created_at`, live carries the daemon ts — so ts can't be the key).
// A turn cctui originated carries a client-minted `turn_id` that the daemon
// stamps on every encoding Claude stores it in, so identity is the primary key
// and no text has to be normalised. The content fallback below stays for the
// turns that can never have one — typed into Claude's own TUI, or persisted
// before the column existed — where user messages collapse across their three
// shapes via `userMsgKey`. Markers (reset/turn_end/heartbeat) key on ts so
// distinct ones aren't over-collapsed.
export function eventSig(e: AgentEvent): string {
	// A queue op shares its text with the prompt it brackets (and its sibling
	// close op), so it needs a signature of its own or the pair collapses.
	if (e.type === 'text' && e.kind === 'queue_op') {
		return `q:${e.operation ?? 'queued'}:${e.seq ?? e.ts}:${e.content.trim()}`;
	}
	if ('turn_id' in e && e.turn_id) return `t:${e.turn_id}`;
	const u = userMsgKey(e);
	if (u !== null) return `u:${u}`;
	switch (e.type) {
		case 'text':
			return `a:${e.content.trim()}`;
		case 'tool_call':
			return `tc:${e.tool}:${JSON.stringify(e.input)}`;
		case 'tool_result':
			return `tr:${e.tool}:${e.output_summary}`;
		case 'compact_summary':
			return `cs:${e.content.trim()}`;
		default:
			return `${e.type}:${e.ts}`;
	}
}

// Merge the drawer's three event sources into one ordered list. `seen` is
// threaded through every source AND across each source's own rows: a duplicate
// inside a single `history` array must collapse too, which a set merely *seeded*
// from that array can never do. `history` is consumed first so it wins over
// `earlier` and `live`.
export function mergeEventSources(
	history: AgentEvent[],
	earlier: AgentEvent[],
	live: AgentEvent[]
): AgentEvent[] {
	const seen = new Set<string>();
	const dedup = (list: AgentEvent[]) =>
		list.filter((e) => {
			const sig = eventSig(e);
			if (seen.has(sig)) return false;
			seen.add(sig);
			return true;
		});
	const hist = dedup(history);
	const front = dedup(earlier);
	const tail = dedup(live);
	return orderEvents([...front, ...hist, ...tail]);
}

// Order the merged history+live event list causally. `seq` is the server's
// monotonic per-session insert sequence (`stream_events.id`), stamped on both
// the reload payload and the live broadcast, so it reflects true causal order
// even when receive-time `ts` ties or inverts — a late-flushed AskUserQuestion
// card+preamble carry a `ts` at/after the user's answer but a LOWER `seq`, so
// ordering by `seq` renders the ask before its answer. Falls back to `ts`
// when either event lacks a `seq`. Uses a stable sort so equal keys keep
// history-before-live order.
export function orderEvents(events: AgentEvent[]): AgentEvent[] {
	return [...events].sort((a, b) => {
		const as = a.seq;
		const bs = b.seq;
		if (as !== null && as !== undefined && bs !== null && bs !== undefined) {
			return Number(as) - Number(bs);
		}
		return Number(a.ts) - Number(b.ts);
	});
}

// Stamp each assistant line with its 1-based conversation turn. A
// turn opens on each user/system prompt to the agent; every assistant line up
// to the next prompt shares it. A `/clear` reset (role 'reset') restarts the
// counter; a `/compact` summary does not. Derived from role transitions, not
// raw index, so out-of-`ts` reloads stay correct. Mutates in place.
export function stampTurns(lines: Line[]): Line[] {
	let turn = 0;
	for (const ln of lines) {
		if (ln.role === 'reset') turn = 0;
		else if (ln.role === 'user' || ln.role === 'system') turn++;
		else if (ln.role === 'assistant') {
			if (turn === 0) turn = 1;
			ln.turn = turn;
		}
	}
	return lines;
}

export function assignLineKeys(lines: Line[]): Line[] {
	const counts = new Map<string, number>();
	for (const ln of lines) {
		const base = `${ln.ts}|${ln.role}|${(ln.text ?? ln.html ?? '').slice(0, 24)}`;
		const n = counts.get(base) ?? 0;
		counts.set(base, n + 1);
		ln.key = n === 0 ? base : `${base}#${n}`;
	}
	return lines;
}

// JSON.stringify only emits \n / \t inside string literals, so expanding them
// for display is safe (display-only — the text is never parsed back).
export function expandJsonEscapes(s: string): string {
	return s.replace(/\\n/g, '\n').replace(/\\t/g, '\t');
}

export function formatToolInput(
	tool: string,
	input: unknown,
	opts: { prettyDiff: boolean; prettyJson: boolean }
): { text: string; lang: string } {
	const obj = input as Record<string, unknown> | null;
	if (opts.prettyDiff && obj && typeof obj === 'object' && 'old_string' in obj && 'new_string' in obj) {
		const minus = String(obj.old_string ?? '')
			.split('\n')
			.map((l) => `- ${l}`)
			.join('\n');
		const plus = String(obj.new_string ?? '')
			.split('\n')
			.map((l) => `+ ${l}`)
			.join('\n');
		return { text: `${obj.file_path ?? ''}\n${minus}\n${plus}`.trim(), lang: '' };
	}
	// Shell-ish tools (Bash, BashOutput, …): render the command itself as a
	// shell block with the description as a leading comment, instead of a
	// one-line JSON blob full of literal "\n" escapes.
	if (opts.prettyJson && obj && typeof obj === 'object' && typeof obj.command === 'string') {
		const desc =
			typeof obj.description === 'string' && obj.description.trim()
				? `# ${obj.description.trim()}\n`
				: '';
		return { text: `${desc}${obj.command}`, lang: 'sh' };
	}
	if (!opts.prettyJson) return { text: JSON.stringify(input), lang: 'json' };
	// Expand escaped newlines/tabs inside string values so multiline payloads
	// (scripts, file contents, heredocs) read as real lines rather than one long
	// "…\n…" run before the highlighter sees them.
	return { text: expandJsonEscapes(prettyJson(input)), lang: 'json' };
}

// Render a single message line as Markdown. Assistant/user/system
// content is already a Markdown source string, so it copies verbatim; tool/result
// code is wrapped in a fenced block (with the tool's language when known).
export function lineMarkdown(ln: Line): string {
	const t = ln.text ?? '';
	if (ln.role === 'tool') {
		const label = ln.tool ? `**${ln.mcp ? 'MCP' : m.turn_tool_label()} · ${ln.tool}**\n\n` : '';
		return `${label}\`\`\`${ln.lang ?? ''}\n${t}\n\`\`\``;
	}
	if (ln.role === 'result') {
		const label = ln.tool ? `**${m.turn_result_label()} · ${ln.tool}**\n\n` : '';
		return `${label}\`\`\`\n${t}\n\`\`\``;
	}
	return t;
}
