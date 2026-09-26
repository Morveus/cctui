import type { TokenUsage as TokenUsageT } from '@bindings/TokenUsage';
import { m } from '$lib/paraglide/messages';
import type { UserUploadRefs } from './lines';

export type MsgCategory =
	| 'assistant'
	| 'thinking'
	| 'redacted'
	| 'attachment'
	| 'user'
	| 'poll'
	| 'peer'
	| 'system'
	| 'tool'
	| 'mcp'
	| 'server_tool'
	| 'result'
	| 'server_result'
	| 'error'
	| 'marker'
	| 'summary'
	| 'compact'
	| 'reset';

export type MsgGroup = 'assistant' | 'user' | 'tools' | 'session';

export type QuickFilterId = 'assistant' | 'user' | 'tools';

export type MsgFilter = Record<MsgCategory, boolean>;

// Resolved at call time so a live language switch re-renders the labels.
export function msgCategoryLabel(id: MsgCategory): string {
	switch (id) {
		case 'assistant':
			return m.conversation_filter_assistant();
		case 'thinking':
			return m.conversation_filter_thinking();
		case 'redacted':
			return m.conversation_filter_redacted();
		case 'attachment':
			return m.conversation_filter_attachment();
		case 'user':
			return m.conversation_filter_user();
		case 'poll':
			return m.conversation_filter_poll();
		case 'peer':
			return m.conversation_filter_peer();
		case 'system':
			return m.conversation_filter_system();
		case 'tool':
			return m.conversation_filter_tool();
		case 'mcp':
			return m.conversation_filter_mcp();
		case 'server_tool':
			return m.conversation_filter_server_tool();
		case 'result':
			return m.conversation_filter_result();
		case 'server_result':
			return m.conversation_filter_server_result();
		case 'error':
			return m.conversation_filter_error();
		case 'marker':
			return m.conversation_filter_marker();
		case 'summary':
			return m.conversation_filter_summary();
		case 'compact':
			return m.conversation_filter_compact();
		case 'reset':
			return m.conversation_filter_reset();
	}
}

export function msgGroupLabel(id: MsgGroup): string {
	switch (id) {
		case 'assistant':
			return m.conversation_filter_group_assistant();
		case 'user':
			return m.conversation_filter_group_user();
		case 'tools':
			return m.conversation_filter_group_tools();
		case 'session':
			return m.conversation_filter_group_session();
	}
}

export function quickFilterLabel(id: QuickFilterId): string {
	switch (id) {
		case 'assistant':
			return m.conversation_filter_quick_assistant();
		case 'user':
			return m.conversation_filter_quick_user();
		case 'tools':
			return m.conversation_filter_quick_tools();
	}
}

export interface ViewOpts {
	msgFilter: MsgFilter;
	prettyJson: boolean;
	prettyDiff: boolean;
	prettyTables: boolean;
	// Desktop drawer width in px (drag-to-resize the left border). Null → the
	// default min(900px, 100vw). Persisted with the other view opts.
	paneWidth: number | null;
}

export interface AskQuestion {
	header?: string;
	question: string;
	multiSelect?: boolean;
	options: { label: string; description?: string; preview?: string }[];
}

export type TodoStatus = 'pending' | 'in_progress' | 'completed';

// Codex's `update_plan` steps carry no gerund form, hence optional `activeForm`.
export interface TodoItem {
	content: string;
	status: TodoStatus;
	activeForm?: string;
	blockedBy?: string[];
}

export interface TodoProgress {
	items: TodoItem[];
	done: number;
	total: number;
	inProgress: TodoItem | null;
}

// Post-turn summary emitted by the server at turn end. Rendered as a footer on
// the turn's last assistant bubble, never as a bubble of its own.
export interface TurnSummary {
	detail: string;
	needsAction: boolean;
	ts: number;
}

export interface Line {
	role:
		| 'assistant'
		| 'thinking'
		| 'user'
		| 'poll'
		| 'peer'
		| 'system'
		| 'marker'
		| 'tool'
		| 'result'
		| 'reset'
		| 'compact'
		| 'summary';
	ts: number;
	html?: string;
	// Pre-highlighted code HTML for the <pre> bubble (tool/result), {@html}.
	htmlCode?: string;
	text?: string;
	// Code language for tool input (sh/json/diff/…), used to fence the
	// copy-as-Markdown output.
	lang?: string;
	tool?: string;
	// Tool calls under the mcp__ prefix get the distinct MCP role hue.
	mcp?: boolean;
	// Thinking whose content the provider withheld: same brown treatment, dimmed.
	redacted?: boolean;
	pending?: boolean;
	// Set on a pending user line that auto-retry is currently re-attempting
	//: shows a "retrying (n/m)" hint instead of plain "sending…".
	retrying?: { attempt: number; max: number };
	// Set on a user line whose send failed: the error reason, shown
	// red with a Retry control.
	failed?: string;
	// This prompt is still sitting in Claude's queue. Cleared by `queuedAt`.
	queued?: boolean;
	// When the prompt was enqueued, on a line that has since been delivered.
	queuedAt?: number;
	// Queued and dropped before delivery.
	cancelled?: boolean;
	// Delivered from the schedule queue: the time it was scheduled for.
	scheduledAt?: number;
	// Parsed AskUserQuestion payload — rendered as interactive cards.
	ask?: AskQuestion[];
	// Parsed ExitPlanMode plan markdown — rendered as a Plan card.
	plan?: string;
	// Parsed TodoWrite / update_plan task list — rendered as a Todo card.
	todos?: TodoItem[];
	// Turn summary attached to this (assistant) line, rendered under its bubble.
	summary?: TurnSummary;
	durationMs?: number;
	key?: string;
	// Server insert sequence (`stream_events.id`). The stable address of a
	// message: `key` is content-derived and `ts` collides.
	seq?: number;
	// 1-based conversation turn; stamped only on assistant lines.
	turn?: number;
	// Consecutive markers collapse into one row; every marker's text is kept
	// here so nothing is lost to the grouping.
	markerTexts?: string[];
	/** A cache keep-alive tick and its reply, folded into one marker row. */
	keepalive?: boolean;
	// `system/stop_hook_summary` for the turn this assistant line closes.
	stopHook?: string;
	// `file-history-snapshot|delta` provenance for this tool call.
	fileHistory?: string[];
	peerFrom?: string;
	// Uploads this turn carried, parsed from the raw text before the harness's
	// attachment encodings were stripped out of the displayed prose.
	uploads?: UserUploadRefs;
	messageId?: string;
	usage?: TokenUsageT;
}
