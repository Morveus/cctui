import type { AgentEvent } from '@bindings/AgentEvent';
import { USER_PREFIX } from '$lib/ws.svelte';
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
	seen: Set<string>;
}

// History stores user turns as a `text` event prefixed with USER_PREFIX; some
// "user" turns are really harness/system messages (detected structurally via
// `looksMeta`) and render in a distinct hue.
function userOrSystem(
	content: string,
	ts: number,
	meta: boolean,
	ctx: LineBuildCtx,
	poll?: PollSeen
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
	let role: Line['role'] = meta ? 'system' : 'user';
	if (looksPoll(content)) {
		role = 'poll';
	} else if (role === 'user' && poll) {
		const norm = normalizePollText(content);
		if (norm) {
			if (poll.seen.has(norm)) role = 'poll';
			else poll.seen.add(norm);
		}
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
		uploads: uploads.names.length ? uploads : undefined
	};
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
					html: ctx.renderMarkdown(e.content),
					text: e.content
				};
			}
			if (e.content.startsWith(USER_PREFIX)) {
				const content = e.content.slice(USER_PREFIX.length).trimStart();
				// Classify structurally from content, not the stored `meta` bit —
				// cctui-injected human replies carry a spurious `isMeta:true` and
				// must stay `user` on reload.
				return userOrSystem(content, Number(e.ts), looksMeta(content), ctx, poll);
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
			return userOrSystem(e.content, Number(e.ts), false, ctx, poll);
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

export function buildLines(
	events: AgentEvent[],
	ctx: LineBuildCtx,
	delivery?: DeliveryState
): Line[] {
	const out: Line[] = [];
	const poll: PollSeen = { seen: new Set() };
	let prevKey = '';
	for (const e of events) {
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
		// Reset/compact markers are keyed by ts so two back-to-back ones aren't
		// collapsed by the consecutive-duplicate guard.
		const key =
			ln.role === 'reset' || ln.role === 'compact'
				? `${ln.role}|${ln.ts}`
				: `${ln.role}|${ln.tool ?? ''}|${(ln.uploads?.names ?? []).join(',')}|${ln.text ?? ln.html ?? ''}|${
						ln.todos ? todoSignature(ln.todos) : ''
					}`;
		if (key === prevKey) continue;
		prevKey = key;
		if ((ln.role === 'user' || ln.role === 'poll') && delivery) {
			if (delivery.pending.has(ln.ts)) ln.pending = true;
			const retry = delivery.retrying.get(ln.ts);
			if (retry !== undefined) ln.retrying = retry;
			const reason = delivery.failed.get(ln.ts);
			if (reason !== undefined) ln.failed = reason;
		}
		out.push(ln);
	}
	// `events` is already ordered causally by `orderEvents` (server insert
	// `seq`), so `out` is built in causal order and rendered as-is — no role
	// grouping, no structural re-anchoring. Ordering by `seq` is what keeps
	// a reloaded AskUserQuestion in [preamble, card, answer] order.
	for (let i = 0; i < out.length; i++) {
		if (out[i].role !== 'assistant') continue;
		const prev = [...out.slice(0, i)]
			.reverse()
			.find((l) => l.role === 'user' || l.role === 'assistant');
		if (prev && out[i].ts > prev.ts) out[i].durationMs = out[i].ts - prev.ts;
	}
	return assignLineKeys(stampTurns(out));
}
