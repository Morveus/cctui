// WS subscription + live-event/permission/ask/delivery state and the send
// orchestration for the conversation drawer, extracted from ConversationDrawer
// with no behavior change. This is the "event context": it owns the live buffer,
// the optimistic-reply echoes, and the per-message delivery tracking, and drives
// the activity ("Working…") indicator.
//
// Live state is kept here (component-local, fed by ws listener callbacks) rather
// than read off the ws singleton from a $derived — a $derived reading the
// singleton's keyed state does NOT re-run on mutation (see ws.svelte.ts header).
import type { AgentEvent } from '@bindings/AgentEvent';
import {
	ws,
	userMsgKey,
	type PermReq,
	type LiveAsk,
	type LivePlan,
	type SoftLimit
} from '$lib/ws.svelte';
import { eventSig, parseAsk, parseTodos, todoProgress as deriveTodoProgress } from './format';
import { lastProseLine, toolInvocationSummary, type ActivityTool } from './activity';
import type { AskQuestion, TodoItem, TodoProgress } from './types';
import { endpoints } from '$lib/queries';

// Fold a live ws event into the cached conversation tail. Returns `prev` by
// identity when nothing changes, so a known event doesn't re-render the drawer.
//
// Both guards protect the `after=` delta cursor. A seq-less event is dropped
// because an optimistic echo's synthetic `maxSeq + 1` is not a `stream_events`
// id and would make the next delta skip real events; an absent or empty entry
// is never seeded because a tail with no base makes the next delta start
// mid-conversation.
export function mergeLiveEvent(
	prev: AgentEvent[] | undefined,
	ev: AgentEvent
): AgentEvent[] | undefined {
	if (!prev?.length) return prev;
	const seq = ev.seq;
	if (seq === null || seq === undefined) return prev;
	const sig = eventSig(ev);
	if (prev.some((e) => e.seq === seq || eventSig(e) === sig)) return prev;
	const at = prev.findIndex((e) => e.seq != null && Number(e.seq) > Number(seq));
	return at < 0 ? [...prev, ev] : [...prev.slice(0, at), ev, ...prev.slice(at)];
}

export interface StreamOpts {
	// The open session id (reactive getter).
	id: () => string;
	// Whether the session is archived (reactive getter) — guards sends.
	archived: () => boolean;
	// Current fetched-history events, for stamping the optimistic reply's ts and
	// seeding `known` (reactive getter).
	historyData: () => AgentEvent[] | undefined;
	// Re-pin the viewport to the bottom (sticky) on send/retry.
	pin: () => void;
	// Invalidate the conversation query (forced refetch on focus/reconnect).
	invalidateConversation: () => void;
	// Invalidate the sessions list (reflect a new turn without waiting for poll).
	invalidateSessions: () => void;
	// Fold a server-sent live event into the `["conversation", sid]` cache.
	mergeIntoCache: (sid: string, ev: AgentEvent) => void;
}

export class ConversationStream {
	#opts: StreamOpts;

	live = $state<AgentEvent[]>([]);
	perms = $state<PermReq[]>([]);
	// Live AskUserQuestion: delivered by the daemon's PreToolUse hook the
	// instant the form renders. Null when none pending.
	ask = $state<LiveAsk | null>(null);
	// Live ExitPlanMode plan prompt: delivered by the daemon's
	// PreToolUse hook the instant the plan-approval prompt renders. Null when
	// none pending. Answered like an ask (digit picks 1-3 / free-text refine).
	plan = $state<LivePlan | null>(null);
	// Per-account soft-limit block: the gateway refused this session's
	// request because cctui's share of the account window is at cap. Drives the
	// per-chat "soft limit reached → continue on another account" banner. Null
	// when no block is active.
	softLimit = $state<SoftLimit | null>(null);
	// Folded last-write-wins: the list mutates many times per turn and only its
	// latest state is meaningful.
	todos = $state<TodoItem[] | null>(null);
	// Activity-banner state. Stamped with the browser clock at receipt rather
	// than the event's daemon `ts`, which is a different clock and would skew
	// every elapsed reading.
	currentTool = $state<ActivityTool | null>(null);
	turnStartedAt = $state<number | null>(null);
	turnTokensIn = $state(0);
	turnTokensOut = $state(0);
	lastAssistantLine = $state<string | null>(null);
	// Per-message delivery state, mirrored from the ws singleton so a
	// failed/in-flight send survives the drawer being reopened.
	pendingReplies = $state<Set<number>>(new Set());
	failedReplies = $state<Map<number, string>>(new Map());
	retryingReplies = $state<Map<number, { attempt: number; max: number }>>(new Map());
	// Activity indicator: true while claude is processing this turn.
	working = $state(false);
	// Optimistic answer lock: locks both ask render sites to their
	// answered state while a reply is in flight.
	answering = $state(false);
	// Bumped to force a full re-subscribe + history refetch (e.g. on tab focus
	// after the ws may have gone half-open while backgrounded).
	resubTick = $state(0);
	// Question texts answered (by us) or resolved (by the daemon) this visit,
	// keyed per session so switching sessions can't cross-suppress.
	resolvedAsks = $state<Set<string>>(new Set());

	constructor(opts: StreamOpts) {
		this.#opts = opts;
	}

	// (Re)subscribe + register listeners for a session. Call from a component
	// $effect that reads `id` and `resubTick` so it re-runs on a session switch or
	// forced resubscribe; the returned teardown unsubscribes. (We drive the effect
	// from the component rather than the constructor so $effect always runs in a
	// proven component-init context.)
	subscribe(sid: string): () => void {
		this.live = ws.bufferedEvents(sid);
		this.answering = false;
		this.working = false;
		this.todos = null;
		this.#resetActivity();
		ws.subscribe(sid);
		const offStream = ws.onStream(sid, (ev) => {
			// Ahead of the echo skip below: the cache must take the server's copy of
			// a user message (it carries the real `seq`) even when `live` keeps the
			// optimistic one instead. The drawer collapses the pair by `eventSig`.
			this.#opts.mergeIntoCache(sid, ev);
			// Skip a server-echoed user message that duplicates our optimistic one.
			const key = userMsgKey(ev);
			if (key !== null && this.live.some((e) => userMsgKey(e) === key)) return;
			this.live = [...this.live, ev];
			// A pending live ask/plan is superseded the instant the agent streams a
			// fresh substantive (non-user, non-heartbeat) event past it:
			// the question was skipped/answered out-of-band and the turn moved on, so
			// the daemon's AskResolved/onAsk(null) — which a half-open ws can miss —
			// is no longer the only thing that clears the form. While a prompt is
			// genuinely pending claude is blocked and emits nothing, so this can't
			// race a still-open question. Remember it resolved so its late transcript
			// line stays suppressed. User echoes (key !== null) never clear
			// a prompt — a free-typed send doesn't dismiss an ask.
			if (ev.type !== 'heartbeat' && key === null) {
				if (this.ask) {
					this.markAsksResolved(this.liveAskQuestions, this.ask.question);
					this.ask = null;
					ws.clearAsk(this.#opts.id());
				}
				if (this.plan) {
					this.plan = null;
					ws.clearPlan(this.#opts.id());
				}
			}
			// Drive the activity indicator: a turn ends on `turn_end`; any
			// substantive agent/tool/user event means work is in progress.
			if (ev.type === 'tool_call' && (ev.tool === 'TodoWrite' || ev.tool === 'update_plan')) {
				const t = parseTodos(ev.input);
				if (t) this.todos = t;
			}
			this.#trackActivity(ev);
			if (ev.type === 'turn_end') this.working = false;
			else if (ev.type !== 'heartbeat') this.working = true;
		});
		const offPerms = ws.onPerms(sid, (list) => {
			this.perms = list;
			// A permission prompt means claude is blocked on the user, not working.
			if (list.length) this.working = false;
		});
		const offAsk = ws.onAsk(sid, (q) => {
			// A pending ask resolving (answered here, from the TUI, or timed out)
			// means its late transcript line must stay suppressed.
			if (!q && this.ask) this.markAsksResolved(this.liveAskQuestions, this.ask.question);
			this.ask = q;
			// A fresh ask (or a resolution) supersedes any in-flight answer lock.
			this.answering = false;
			// A pending question means claude is waiting on the user, not working.
			if (q) this.working = false;
		});
		const offPlan = ws.onPlan(sid, (p) => {
			this.plan = p;
			// A fresh plan (or a resolution) supersedes any in-flight answer lock.
			this.answering = false;
			// A pending plan means claude is waiting on the user, not working.
			if (p) this.working = false;
		});
		const offSoftLimit = ws.onSoftLimit(sid, (sl) => {
			this.softLimit = sl;
		});
		// Mirror the singleton's per-session delivery state. Fires
		// immediately with the current snapshot and on every ack / auto-retry.
		const offDelivery = ws.onDelivery(sid, (snap) => {
			this.pendingReplies = snap.pending;
			this.failedReplies = snap.failed;
			this.retryingReplies = snap.retrying;
			// A failed answer must not leave the ask sites locked "Answering…"
			// forever.
			if (snap.failed.size) this.answering = false;
		});
		return () => {
			offStream();
			offPerms();
			offAsk();
			offPlan();
			offSoftLimit();
			offDelivery();
			ws.unsubscribe(sid);
			ws.clearStream(sid);
		};
	}

	// When the tab is backgrounded the ws can go half-open and miss events; on
	// return force a fresh history refetch + re-subscribe (via resubTick) so the
	// chat catches up. Call from a component $effect; returns the listener teardown.
	installVisibilityRefresh(): () => void {
		const refresh = () => {
			if (typeof document !== 'undefined' && document.visibilityState === 'hidden') return;
			ws.connect();
			this.#opts.invalidateConversation();
			this.resubTick++;
		};
		const onVis = () => {
			if (document.visibilityState === 'visible') refresh();
		};
		document.addEventListener('visibilitychange', onVis);
		window.addEventListener('focus', refresh);
		return () => {
			document.removeEventListener('visibilitychange', onVis);
			window.removeEventListener('focus', refresh);
		};
	}

	#resetActivity() {
		this.currentTool = null;
		this.turnStartedAt = null;
		this.turnTokensIn = 0;
		this.turnTokensOut = 0;
		this.lastAssistantLine = null;
	}

	#trackActivity(ev: AgentEvent) {
		if (ev.type === 'turn_end') {
			this.currentTool = null;
			this.turnStartedAt = null;
			return;
		}
		if (ev.type === 'heartbeat') return;
		if (this.turnStartedAt === null) {
			this.turnStartedAt = Date.now();
			this.turnTokensIn = 0;
			this.turnTokensOut = 0;
		}
		switch (ev.type) {
			case 'tool_call':
				this.currentTool = {
					tool: ev.tool,
					summary: toolInvocationSummary(ev.tool, ev.input),
					startedAt: Date.now()
				};
				break;
			case 'tool_result':
				// Only the matching tool clears the banner: a late result for an
				// earlier call must not blank a tool that has already started.
				if (this.currentTool?.tool === ev.tool) this.currentTool = null;
				break;
			case 'text': {
				if (ev.usage) {
					this.turnTokensIn += ev.usage.tokens_in;
					this.turnTokensOut += ev.usage.tokens_out;
				}
				if (ev.kind) break;
				const line = lastProseLine(ev.content);
				if (line) {
					this.lastAssistantLine = line;
					this.currentTool = null;
				}
				break;
			}
			case 'reply':
				this.lastAssistantLine = null;
				this.currentTool = null;
				break;
			case 'context_reset':
				this.#resetActivity();
				break;
		}
	}

	// Null when the session never produced a task list — callers render nothing
	// rather than a zero state.
	get todoProgress(): TodoProgress | null {
		return deriveTodoProgress(this.todos);
	}

	// Parsed structured questions for the live prompt, or null → text fallback.
	get liveAskQuestions(): AskQuestion[] | null {
		return this.ask?.questions ? parseAsk({ questions: this.ask.questions }) : null;
	}

	#askKey = (q: string) => `${this.#opts.id()}|${q}`;
	markAsksResolved(qs: { question: string }[] | null, fallback?: string) {
		const next = new Set(this.resolvedAsks);
		for (const q of qs ?? []) next.add(this.#askKey(q.question));
		if (!qs?.length && fallback) next.add(this.#askKey(fallback));
		this.resolvedAsks = next;
	}

	// Dedupe the two ask render sites: suppress a transcript ask line
	// that carries the same question as a pending/resolved live ask.
	isDupeOfLiveAsk = (a: AskQuestion[]): boolean => {
		const q = a[0]?.question;
		if (!q) return false;
		// Already answered/resolved this session.
		if (this.resolvedAsks.has(this.#askKey(q))) return true;
		if (!this.ask) return false;
		const liveQ = this.liveAskQuestions?.[0]?.question;
		if (liveQ !== undefined) return q === liveQ;
		// Text-only fallback delivery: the flattened `question` embeds the text.
		return this.ask.question.includes(q);
	};

	// Optimistic echo of a user-typed message. Kept in the ws singleton (not just
	// local `live`) so a resubscribe/reconnect that rebuilds `live` doesn't drop a
	// message claude already received. Stamps the echo just past the newest known
	// event rather than with the browser clock so it sorts last.
	#pushOptimisticReply(text: string): number {
		const id = this.#opts.id();
		const known = [...(this.#opts.historyData() ?? []), ...this.live];
		const maxTs = known.reduce((m, e) => Math.max(m, e.ts), 0);
		const ts = Math.max(Date.now(), maxTs + 1);
		// Stamp a causal `seq` just past the newest known event so the
		// echo sorts last under `orderEvents`, matching the `ts` bump above.
		const maxSeq = known.reduce((m, e) => Math.max(m, e.seq ?? 0), 0);
		const ev: AgentEvent = { type: 'reply', content: text, ts, seq: maxSeq + 1 };
		ws.recordOptimistic(id, ev);
		this.live = [...this.live, ev];
		return ts;
	}

	// Create the optimistic echo and hand the send off to the ws singleton (which
	// owns dispatch + ack timeout + auto-retry). Returns true if the first frame
	// left the socket.
	#sendTracked(text: string): boolean {
		const ts = this.#pushOptimisticReply(text);
		return ws.trackedSend(this.#opts.id(), text, ts);
	}

	// Send a final message body (text + any appended staged-attachment paths).
	// NB: a free-typed send does NOT dismiss a pending AskUserQuestion.
	sendBody(text: string) {
		if (!text || this.#opts.archived()) return;
		// Sending always jumps to the latest message (classic chat UX).
		this.#opts.pin();
		const ok = this.#sendTracked(text);
		if (ok) this.working = true;
		this.#opts.invalidateSessions();
	}

	// Re-send a failed message manually (resets the auto-retry counter). The
	// optimistic echo keeps its `ts`, so the bubble stays put.
	retryFailed(ts: number) {
		if (this.#opts.archived()) return;
		this.working = true;
		this.#opts.pin();
		ws.retryNow(this.#opts.id(), ts);
	}

	// Answer an AskUserQuestion. With pure option picks the daemon drives
	// the real form via PTY keystrokes; the flattened text rides along as
	// the carrier for the free-text/fallback path.
	answerQuestion(text: string, picks: number[][] | null, qs?: AskQuestion[] | null) {
		if (this.#opts.archived()) return;
		const ts = this.#pushOptimisticReply(text);
		const ok = ws.trackedSend(this.#opts.id(), text, ts, picks ?? undefined);
		if (!ok) return;
		// Lock both ask render sites to their answered state immediately,
		// and remember the answered questions so the late transcript line never
		// resurfaces as a fresh form.
		this.markAsksResolved(qs ?? this.liveAskQuestions, this.ask?.question);
		this.answering = true;
		// Answering hands control back to claude — show the working indicator.
		this.working = true;
		// Dismiss the live prompt immediately — the daemon's AskResolved arrives a
		// poll later.
		this.ask = null;
		ws.clearAsk(this.#opts.id());
		this.#opts.invalidateSessions();
	}

	// Answer an ExitPlanMode plan prompt. A pure pick (1-3) drives the
	// real PTY form natively via keystrokes (the daemon stores a synthetic
	// single-select form in its pending-ask map); a free-text refinement (the
	// "Tell Claude what to change" option) takes the dismiss-then-reply path with
	// `picks = null`. Same trackedSend grammar as `answerQuestion`.
	answerPlan(text: string, picks: number[][] | null) {
		if (this.#opts.archived()) return;
		const ts = this.#pushOptimisticReply(text);
		const ok = ws.trackedSend(this.#opts.id(), text, ts, picks ?? undefined);
		if (!ok) return;
		this.answering = true;
		this.working = true;
		this.plan = null;
		ws.clearPlan(this.#opts.id());
		this.#opts.invalidateSessions();
	}

	// Switch this session's account after a soft-limit block. The
	// gateway resolves the worker's opaque token to an account per request, so a
	// switch is a pure server-side rebind — the worker keeps running and its next
	// upstream call lands on the target. Dismiss the banner optimistically; the
	// server's `soft_limit_cleared` confirms a moment later. On failure (e.g.
	// provider mismatch → 409) the banner stays and the error is surfaced.
	async switchAccount(account: string): Promise<void> {
		const id = this.#opts.id();
		await endpoints.switchAccount(id, account);
		this.softLimit = null;
		ws.clearSoftLimit(id);
		this.#opts.invalidateSessions();
	}

	// Discard a still-pending optimistic echo (edit/recover). Stops
	// tracking/retrying the send and drops the echo from both the local
	// buffer and the ws optimistic store. The caller pulls the recovered
	// text into the composer.
	discardOptimistic(ts: number) {
		const id = this.#opts.id();
		ws.cancelSend(id, ts);
		ws.dropOptimistic(id, ts);
		this.live = this.live.filter((e) => !(e.type === 'reply' && e.ts === ts));
	}
}
