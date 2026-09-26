import type { AgentEvent } from '@bindings/AgentEvent';
import { USER_PREFIX } from '$lib/ws.svelte';
import { looksKeepaliveTick } from './keepalive';
import { m } from '$lib/paraglide/messages';
import {
	assignLineKeys,
	formatToolInput,
	looksMeta,
	parseAsk,
	IMAGE_TOKEN_RUN_RE,
	isSyntheticImageNotice,
	parsePeerMessage,
	parsePlan,
	parseTodos,
	stampTurns,
	todoSignature,
	stripAttachmentDecorations
} from './format';
import type { Line, MsgCategory } from './types';

export interface LineBuildCtx {
	visible: (c: MsgCategory) => boolean;
	renderMarkdown: (s: string) => string;
	renderCode: (text: string, lang: string) => string;
	prettyJson: boolean;
	prettyDiff: boolean;
	/** turn_id → deliver_at of delivered scheduled messages. */
	scheduledTurns?: ReadonlyMap<string, string>;
}

export interface DeliveryState {
	pending: Set<number>;
	failed: Map<number, string>;
	retrying: Map<number, { attempt: number; max: number }>;
}

// `# Autonomous loop` is also in `META_TAGS` and the daemon's `META_MARKERS`.
const POLL_PREFIXES = ['# Autonomous loop'];
const POLL_SENTINELS = ['<<autonomous-loop>>', '<<autonomous-loop-dynamic>>'];

export function looksPoll(text: string): boolean {
	const t = text.trimStart();
	return (
		POLL_PREFIXES.some((mk) => t.startsWith(mk)) || POLL_SENTINELS.some((mk) => t.includes(mk))
	);
}

export function normalizePollText(text: string): string {
	return text.trim().replace(/\s+/g, ' ');
}

export interface PollSeen {
	/** Normalised text of the immediately preceding user turn. */
	last: string | null;
}

export function newPollSeen(): PollSeen {
	return { last: null };
}

// Only a *consecutive* repeat with no `turn_id` demotes: cctui stamps a
// `turn_id` on everything a human sends, a monitor re-injection never carries
// one, and anything looser demotes a human typing `continue` twice.
export function pollDuplicate(
	content: string,
	turnId: string | null | undefined,
	state: PollSeen
): boolean {
	const norm = normalizePollText(content);
	if (!norm) return false;
	const dup = !turnId && state.last === norm;
	state.last = norm;
	return dup;
}

export function breaksPollRun(e: AgentEvent): boolean {
	switch (e.type) {
		case 'text':
			if (e.kind === 'turn_annotation' || e.kind === 'system_marker') return false;
			if (e.kind === 'queue_op') return false;
			return !e.content.startsWith(USER_PREFIX);
		case 'tool_call':
		case 'tool_result':
		case 'context_reset':
		case 'compact_summary':
			return true;
		default:
			return false;
	}
}

// History stores user turns as a `text` event prefixed with USER_PREFIX; some
// "user" turns are really harness/system messages (detected structurally via
// `looksMeta`) and render in a distinct hue.
function userOrSystem(
	content: string,
	ts: number,
	meta: boolean,
	ctx: LineBuildCtx,
	poll?: PollSeen,
	turnId?: string | null,
	scheduledAt?: string | null
): Line | null {
	const peer = parsePeerMessage(content);
	if (peer) {
		if (!ctx.visible('peer')) return null;
		return {
			role: 'peer',
			ts,
			html: ctx.renderMarkdown(peer.body),
			text: peer.body,
			peerFrom: peer.from ?? undefined
		};
	}
	if (looksKeepaliveTick(content)) {
		if (!ctx.visible('marker')) return null;
		const label = m.conversation_keepalive_tick();
		return { role: 'marker', ts, text: label, markerTexts: [label], keepalive: true };
	}
	let role: Line['role'] = meta ? 'system' : 'user';
	if (scheduledAt) {
		role = 'user';
	} else if (looksPoll(content)) {
		role = 'poll';
	} else if (role === 'user' && poll && pollDuplicate(content, turnId, poll)) {
		role = 'poll';
	}
	if (!ctx.visible(role)) return null;
	// Claude's synthetic `[Image: source: …]` turn carries no human content; it
	// exists only to echo what it ingested, and rendering it duplicates the turn.
	if (isSyntheticImageNotice(content)) return null;
	const uploads = parseUserUploadRefs(content);
	const prose = stripAttachmentDecorations(content);
	return {
		role,
		ts,
		html: prose ? ctx.renderMarkdown(prose) : '',
		text: prose,
		uploads: uploads.names.length ? uploads : undefined,
		scheduledAt: scheduledAt ? Date.parse(scheduledAt) : undefined
	};
}

export function scheduledAtOf(e: AgentEvent, ctx: LineBuildCtx): string | null {
	const meta = (e as { metadata?: { scheduled_at?: unknown } }).metadata;
	if (typeof meta?.scheduled_at === 'string') return meta.scheduled_at;
	const turnId = 'turn_id' in e ? e.turn_id : null;
	return (turnId && ctx.scheduledTurns?.get(turnId)) || null;
}

// Errors win so one toggle isolates every failed result, server or client.
export function resultCategory(e: AgentEvent & { type: 'tool_result' }): MsgCategory {
	return e.error ? 'error' : e.kind === 'server_tool_result' ? 'server_result' : 'result';
}

export function toLine(e: AgentEvent, ctx: LineBuildCtx, poll?: PollSeen): Line | null {
	const ln = buildLine(e, ctx, poll);
	if (ln && typeof e.seq === 'number') ln.seq = e.seq;
	return ln;
}

function buildLine(e: AgentEvent, ctx: LineBuildCtx, poll?: PollSeen): Line | null {
	switch (e.type) {
		case 'text': {
			// Streaming emits an empty text event before the populated one — skip
			// empties so they don't render as blank assistant blocks.
			if (!e.content.trim()) return null;
			if (e.kind === 'thinking' || e.kind === 'redacted_thinking') {
				const redacted = e.kind === 'redacted_thinking';
				if (!ctx.visible(redacted ? 'redacted' : 'thinking')) return null;
				return {
					role: 'thinking',
					ts: Number(e.ts),
					html: ctx.renderMarkdown(e.content),
					text: e.content,
					redacted
				};
			}
			// Markers carry no USER_PREFIX, so they must be claimed before the
			// assistant fallthrough or they read as assistant prose.
			if (e.kind === 'system_marker') {
				if (!ctx.visible('marker')) return null;
				return {
					role: 'marker',
					ts: Number(e.ts),
					text: e.content,
					markerTexts: [e.content]
				};
			}
			// Turn annotations are never lines of their own; `buildLines`
			// re-anchors them onto the turn they describe.
			if (e.kind === 'turn_annotation') return null;
			if (e.content.startsWith(USER_PREFIX)) {
				const content = e.content.slice(USER_PREFIX.length).trimStart();
				// Classify structurally from content, not the stored `meta` bit —
				// cctui-injected human replies carry a spurious `isMeta:true` and
				// must stay `user` on reload.
				const scheduledAt = scheduledAtOf(e, ctx);
				return userOrSystem(
					content,
					Number(e.ts),
					!scheduledAt && looksMeta(content),
					ctx,
					poll,
					e.turn_id,
					scheduledAt
				);
			}
			if (!ctx.visible(e.kind === 'attachment' ? 'attachment' : 'assistant')) return null;
			return {
				role: 'assistant',
				ts: Number(e.ts),
				html: ctx.renderMarkdown(e.content),
				text: e.content,
				messageId: e.message_id ?? undefined,
				usage: e.usage ?? undefined
			};
		}
		case 'reply':
			// `reply` is only ever our own optimistic echo of typed input.
			if (!e.content.trim()) return null;
			return userOrSystem(e.content, Number(e.ts), false, ctx, poll, e.turn_id);
		case 'tool_call': {
			if (e.tool === 'AskUserQuestion') {
				const ask = parseAsk(e.input);
				if (ask) return { role: 'tool', ts: Number(e.ts), tool: e.tool, ask };
			}
			if (e.tool === 'ExitPlanMode') {
				const plan = parsePlan(e.input);
				if (plan) return { role: 'tool', ts: Number(e.ts), tool: e.tool, plan };
			}
			if (e.tool === 'TodoWrite' || e.tool === 'update_plan') {
				const todos = parseTodos(e.input);
				if (todos) return { role: 'tool', ts: Number(e.ts), tool: e.tool, todos };
			}
			const isMcp = e.tool.startsWith('mcp__');
			const cat = e.kind === 'server_tool_use' ? 'server_tool' : isMcp ? 'mcp' : 'tool';
			if (!ctx.visible(cat)) return null;
			const { text, lang } = formatToolInput(e.tool, e.input, {
				prettyDiff: ctx.prettyDiff,
				prettyJson: ctx.prettyJson
			});
			return {
				role: 'tool',
				ts: Number(e.ts),
				tool: e.tool,
				mcp: isMcp,
				text,
				lang,
				htmlCode: ctx.renderCode(text, lang)
			};
		}
		case 'tool_result':
			if (!ctx.visible(resultCategory(e))) return null;
			return {
				role: 'result',
				ts: Number(e.ts),
				tool: e.tool,
				text: e.output_summary,
				htmlCode: ctx.renderCode(e.output_summary, '')
			};
		case 'context_reset':
			// /clear: the session id rotated under the same worker.
			if (!ctx.visible('reset')) return null;
			return { role: 'reset', ts: Number(e.ts), text: m.conversation_context_reset() };
		case 'compact_summary':
			// /compact appends a summary in place (no session-id rotation), so it
			// arrives with its text.
			if (!ctx.visible('compact')) return null;
			if (!e.content.trim()) return null;
			return {
				role: 'compact',
				ts: Number(e.ts),
				html: ctx.renderMarkdown(e.content),
				text: e.content
			};
		default:
			return null; // heartbeat, turn_end, turn_summary
	}
}

// A turn summary belongs to the last assistant bubble of its turn. Scan back
// only to the turn boundary (a user/system prompt or a /clear), so a summary
// never lands on an assistant message from an earlier turn.
function attachSummary(out: Line[], e: AgentEvent & { type: 'turn_summary' }): Line | null {
	const detail = e.detail.trim() || (e.status_category ?? '').trim();
	if (!detail) return null;
	const summary = { detail, needsAction: e.needs_action, ts: Number(e.ts) };
	for (let i = out.length - 1; i >= 0; i--) {
		const ln = out[i];
		if (ln.role === 'assistant' && !ln.summary) {
			ln.summary = summary;
			return null;
		}
		if (ln.role === 'user' || ln.role === 'system' || ln.role === 'reset') break;
	}
	// No assistant bubble to hang it on (filtered out, or paged away): keep it as
	// a standalone footer rather than dropping it.
	return { role: 'summary', ts: summary.ts, summary, text: detail };
}

// Annotations arrive as `<kind>[:<detail>]` in a `turn_annotation` text event.
export function parseAnnotation(content: string): { kind: string; detail: string } {
	const at = content.indexOf(':');
	return at === -1
		? { kind: content, detail: '' }
		: { kind: content.slice(0, at), detail: content.slice(at + 1) };
}

// Scans back to the nearest line of `roles`, stopping at a turn boundary so an
// annotation never lands on an earlier turn.
function ownerLine(out: Line[], roles: Line['role'][], stopAt: Line['role'][]): Line | null {
	for (let i = out.length - 1; i >= 0; i--) {
		const ln = out[i];
		if (roles.includes(ln.role)) return ln;
		if (stopAt.includes(ln.role)) break;
	}
	return null;
}

function attachAnnotation(out: Line[], content: string): void {
	const { kind, detail } = parseAnnotation(content);
	switch (kind) {
		case 'turn_duration': {
			const ms = Number(detail);
			const owner = ownerLine(out, ['assistant'], ['user', 'poll', 'peer', 'reset']);
			if (owner && Number.isFinite(ms) && ms > 0) owner.durationMs = ms;
			return;
		}
		case 'stop_hook_summary': {
			const owner = ownerLine(out, ['assistant'], ['user', 'poll', 'peer', 'reset']);
			if (owner && detail) owner.stopHook = detail;
			return;
		}
		case 'file_history': {
			const owner = ownerLine(out, ['tool'], ['user', 'poll', 'peer', 'reset']);
			if (owner && detail) {
				owner.fileHistory ??= [];
				owner.fileHistory.push(detail);
			}
			return;
		}
	}
}

// What the composer wrote for a message that carried uploads: `[name]` tokens
// in the prose plus an "Attached file(s)" block listing the daemon's staged
// paths (`/tmp/cctui-uploads/<session>/<name>`). The session id is taken from
// those paths so the line can resolve its blobs without the drawer threading
// it through. Mirrors the composer's naming in `$lib/attachments`.
export const PASTE_NAME_RE = /^paste-\d+\.txt$/;
const STAGED_PATH_RE = /^- \/tmp\/cctui-uploads\/([^/\s]+)\/(\S.*?)\s*$/;
const BRACKET_TOKEN_RE = /\[([^[\]\n]+\.[A-Za-z0-9]{1,8})\]/g;

export interface UserUploadRefs {
	sessionId: string | null;
	names: string[];
}

export function isPasteName(name: string): boolean {
	return PASTE_NAME_RE.test(name);
}

export function parseUserUploadRefs(text: string | undefined): UserUploadRefs {
	const names: string[] = [];
	let sessionId: string | null = null;
	if (!text) return { sessionId, names };
	const seen = new Set<string>();
	const push = (n: string) => {
		if (!seen.has(n)) {
			seen.add(n);
			names.push(n);
		}
	};
	for (const line of text.split('\n')) {
		const st = STAGED_PATH_RE.exec(line);
		if (st) {
			sessionId ??= st[1];
			push(st[2]);
		}
	}
	// The staged block is authoritative: staging renames a colliding name, and a
	// `[paste-1.txt]` token the composer left on the old one resolves by name to
	// another message's upload.
	if (names.length) return { sessionId, names };
	for (const m of text.matchAll(BRACKET_TOKEN_RE)) {
		if (isPasteName(m[1])) push(m[1]);
	}
	// Claude's own copy of the turn carries no staged paths, only a leading run
	// of `[Image #N][name]` tokens. Names are taken from that run alone: a
	// `[name.ext]` later in the prose is the human's own text, not an upload.
	const run = IMAGE_TOKEN_RUN_RE.exec(text);
	if (run) {
		for (const m of run[0].matchAll(BRACKET_TOKEN_RE)) push(m[1]);
	}
	return { sessionId, names };
}

// Claude's `queue-operation` records carry no queue id, so a queued prompt is
// correlated with its delivered turn by text. `excerpt` keeps only the first
// line (120 chars on rows written before `queue_text`), hence first-line prefix
// matching on normalised text rather than equality.
// The prose is keyed, not the decorations: a user line has already had its
// `[name]` token run stripped while the enqueue row still carries it.
export function queueKey(text: string | undefined): string {
	const first = stripAttachmentDecorations(text ?? '').split('\n')[0] ?? '';
	return normalizePollText(first).replace(/…$/, '');
}

interface QueueClose {
	key: string;
	absorbed: boolean;
}

// A real user line absorbs the placeholder it matches: the real event's `seq`
// wins, because pins, forks and jump-to-seq address user messages by `seq` and
// the enqueue row's id must never leak into them.
function reconcileQueued(out: Line[], placeholders: Line[], closes: QueueClose[]): Line[] {
	if (!placeholders.length) return out;
	const open = [...placeholders];
	const absorbed = new Set<Line>();
	for (const ln of out) {
		if (ln.queued || ln.role !== 'user') continue;
		const first = queueKey(ln.text);
		if (!first) continue;
		const at = open.findIndex((p) => {
			const key = queueKey(p.text);
			return key.length > 0 && first.startsWith(key);
		});
		if (at === -1) continue;
		const [ph] = open.splice(at, 1);
		ln.queuedAt = ph.ts;
		absorbed.add(ph);
	}
	// A `dequeue` record never carries its content, so a bodiless close is only
	// consumed once no texted close claims the placeholder.
	const texted = closes.filter((c) => c.key);
	const bodiless = closes.filter((c) => !c.key);
	for (const ph of open) {
		const key = queueKey(ph.text);
		const at = texted.findIndex((c) => key && (c.key.startsWith(key) || key.startsWith(c.key)));
		const close = at !== -1 ? texted.splice(at, 1)[0] : bodiless.shift();
		if (!close) continue;
		if (close.absorbed) ph.queuedAt = ph.ts;
		else ph.cancelled = true;
	}
	return out.filter((l) => !absorbed.has(l));
}

export function buildLines(
	events: AgentEvent[],
	ctx: LineBuildCtx,
	delivery?: DeliveryState
): Line[] {
	const out: Line[] = [];
	const poll = newPollSeen();
	const placeholders: Line[] = [];
	const closes: QueueClose[] = [];
	let prevKey = '';
	let hiddenTick = false;
	for (const e of events) {
		if (breaksPollRun(e)) poll.last = null;
		if (
			e.type === 'text' &&
			e.content.startsWith(USER_PREFIX) &&
			looksKeepaliveTick(e.content.slice(USER_PREFIX.length)) &&
			!ctx.visible('marker')
		) {
			hiddenTick = true;
			continue;
		}
		if (e.type === 'text' && e.kind === 'queue_op') {
			const body = e.content.trim();
			const op = e.operation ?? 'queued';
			if (op !== 'queued') {
				closes.push({ key: queueKey(body), absorbed: op === 'absorbed' });
				continue;
			}
			if (!body || !ctx.visible('user')) continue;
			const ln: Line = {
				role: 'user',
				ts: Number(e.ts),
				text: body,
				html: ctx.renderMarkdown(body),
				queued: true
			};
			if (typeof e.seq === 'number') ln.seq = e.seq;
			placeholders.push(ln);
			out.push(ln);
			// A placeholder must stay invisible to the consecutive-duplicate guard:
			// it keeps the FIRST line, which would hand the real user bubble the
			// enqueue row's `seq`.
			prevKey = '';
			continue;
		}
		if (e.type === 'text' && e.kind === 'turn_annotation') {
			attachAnnotation(out, e.content);
			continue;
		}
		if (e.type === 'turn_summary') {
			if (!ctx.visible('summary')) continue;
			const orphan = attachSummary(out, e);
			// An attached summary is not a line, so it must stay invisible to the
			// consecutive-duplicate guard below.
			if (orphan) {
				out.push(orphan);
				prevKey = `summary|${orphan.ts}`;
			}
			continue;
		}
		const ln = toLine(e, ctx, poll);
		if (!ln) continue;
		if (hiddenTick && (ln.role === 'assistant' || ln.role === 'thinking')) continue;
		hiddenTick = false;
		// The three encodings Claude stores ONE human turn in share its `turn_id`,
		// so keying on it still collapses them while two composer sends of the same
		// text — always distinct ids — stay two messages.
		const turnId = (e.type === 'text' || e.type === 'reply' ? e.turn_id : null) ?? '';
		// Reset/compact markers are keyed by ts so two back-to-back ones aren't
		// collapsed by the consecutive-duplicate guard.
		const key =
			ln.role === 'reset' || ln.role === 'compact'
				? `${ln.role}|${ln.ts}`
				: `${ln.role}|${ln.tool ?? ''}|${(ln.uploads?.names ?? []).join(',')}|${ln.text ?? ln.html ?? ''}|${
						ln.todos ? todoSignature(ln.todos) : ''
					}|${turnId}`;
		if (key === prevKey) continue;
		prevKey = key;
		// Consecutive markers collapse into one row: they arrive in bursts at the
		// same second and each is a single line of bookkeeping.
		const prevLine = out[out.length - 1];
		if (prevLine?.keepalive && (ln.role === 'assistant' || ln.role === 'thinking')) {
			if (ln.role === 'assistant' && ln.text?.trim()) {
				prevLine.markerTexts = [...(prevLine.markerTexts ?? []), ln.text.trim()];
				prevLine.text = prevLine.markerTexts.join(' · ');
			}
			continue;
		}
		if (ln.role === 'marker' && prevLine?.role === 'marker') {
			prevLine.keepalive = prevLine.keepalive || ln.keepalive;
			prevLine.markerTexts = [...(prevLine.markerTexts ?? []), ...(ln.markerTexts ?? [])];
			prevLine.text = prevLine.markerTexts.join(' · ');
			continue;
		}
		if ((ln.role === 'user' || ln.role === 'poll') && delivery) {
			if (delivery.pending.has(ln.ts)) ln.pending = true;
			const retry = delivery.retrying.get(ln.ts);
			if (retry !== undefined) ln.retrying = retry;
			const reason = delivery.failed.get(ln.ts);
			if (reason !== undefined) ln.failed = reason;
		}
		out.push(ln);
	}
	const lines = reconcileQueued(out, placeholders, closes);
	// `events` is already ordered causally by `orderEvents` (server insert
	// `seq`), so `out` is built in causal order and rendered as-is — no role
	// grouping, no structural re-anchoring. Ordering by `seq` is what keeps
	// a reloaded AskUserQuestion in [preamble, card, answer] order.
	for (let i = 0; i < lines.length; i++) {
		if (lines[i].role !== 'assistant') continue;
		const prev = [...lines.slice(0, i)]
			.reverse()
			.find((l) => l.role === 'user' || l.role === 'assistant');
		// A `system/turn_duration` annotation is exact; only estimate without one.
		if (lines[i].durationMs !== undefined) continue;
		if (prev && lines[i].ts > prev.ts) lines[i].durationMs = lines[i].ts - prev.ts;
	}
	return assignLineKeys(stampTurns(lines));
}
