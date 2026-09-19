import type { Label } from '@bindings/Label';
import type { SessionListItem } from '@bindings/SessionListItem';
import { escapeHtml } from '$lib/markdown';
import { m } from '$lib/paraglide/messages';
import { highlightTerms } from '$lib/search';
import { type SessionEnd, sessionEnd } from '$lib/sessionEnd';
import {
	type ToolActivity,
	branchOf,
	isStaleWorking,
	remoteOf,
	toolActivity
} from '../../../../routes/sessions/sessions.logic';

export type SubagentToggle = {
	key: string;
	count: number;
	running: number;
	open: boolean;
	label: string;
	ontoggle: () => void;
};

export type PrLink = { href: string; label: string };

export interface SessionView {
	s: SessionListItem;
	child: boolean;
	showMachine: boolean;
	title: string;
	lastMsg: string | null;
	snippetHtml: string | null;
	now: number;
	stale: boolean;
	act: ToolActivity;
	livenessClass: string;
	needsInput: boolean;
	end: SessionEnd | null;
	showStatusBadge: boolean;
	branch: string | null;
	remote: string | null;
	prLinks: PrLink[];
	rollup: { tokens: number; count: number } | null;
	pendingCount: number;
	unreadCount: number;
	draft: boolean;
	draftLaunching: boolean;
}

export interface SessionActions {
	selectable: boolean;
	selected: boolean;
	subagentToggles: SubagentToggle[];
	onTogglePin?: (s: SessionListItem) => void;
	labelEditable: boolean;
	allLabels: Label[];
	onCreateLabel?: (name: string, color: string) => Promise<Label>;
	onAttachLabel?: (id: string, labelId: string) => void | Promise<void>;
	onDetachLabel?: (id: string, labelId: string) => void | Promise<void>;
	onUpdateLabel?: (labelId: string, patch: { name?: string; color?: string }) => Promise<Label>;
	onDeleteLabel?: (labelId: string) => void | Promise<void>;
	onLaunch?: (s: SessionListItem) => void;
	onEdit?: (s: SessionListItem) => void;
	onDiscard?: (s: SessionListItem) => void;
}

export function prLinksOf(s: SessionListItem): PrLink[] {
	return (s.pr_links ?? []).map((href) => {
		const parts = href.replace(/\/+$/, '').split('/');
		const i = parts.findIndex((p) => p === 'pull' || p === 'pulls');
		const label =
			i >= 2 && parts[i + 1] ? `${parts[i - 2]}/${parts[i - 1]}#${parts[i + 1]}` : href;
		return { href, label };
	});
}

export function livenessClassOf(s: SessionListItem, stale: boolean): string {
	if (s.hibernated) return 'dot-hibernated';
	if (stale || s.liveness === 'stale') return 'dot-stale';
	return s.liveness === 'active' ? 'dot-active' : 'dot-dead';
}

// Subagents inherit the parent's working dir, so nameless children get the
// short id instead of the dir basename every sibling would share.
export function titleOf(s: SessionListItem, child: boolean): string {
	const dirName = s.working_dir.split('/').filter(Boolean).pop() || '';
	return s.name || (child ? s.id.slice(0, 6) : dirName || s.id);
}

export function statusLabel(st: string): string {
	switch (st) {
		case 'new':
			return m.sessions_status_new();
		case 'archived':
			return m.sessions_status_archived();
		case 'active':
			return m.sessions_status_active();
		case 'inactive':
			return m.sessions_status_inactive();
		case 'dead':
			return m.sessions_status_dead();
		case 'draft':
			return m.sessions_status_draft();
		case 'queued':
			return m.sessions_status_queued();
		default:
			return st;
	}
}

export function buildView(
	s: SessionListItem,
	opts: {
		child: boolean;
		showMachine: boolean;
		now: number;
		preview: string | null;
		highlight: string[];
		subagentCost: { tokens: number; count: number } | null;
		pendingCount: number;
		unreadCount: number;
		draft: boolean;
		draftLaunching: boolean;
	}
): SessionView {
	const stale = isStaleWorking(s, opts.now);
	return {
		s,
		child: opts.child,
		showMachine: opts.showMachine,
		title: titleOf(s, opts.child),
		lastMsg: opts.preview ?? s.last_message_text ?? null,
		snippetHtml:
			s.match_snippet && opts.highlight.length
				? highlightTerms(escapeHtml(s.match_snippet), opts.highlight)
				: null,
		now: opts.now,
		stale,
		act: toolActivity(s, opts.now),
		livenessClass: livenessClassOf(s, stale),
		needsInput: s.attention === 'needs_input' && s.status !== 'archived',
		end: sessionEnd(s),
		showStatusBadge: s.status === 'new' || s.status === 'archived' || s.status === 'queued',
		branch: branchOf(s),
		remote: remoteOf(s),
		prLinks: prLinksOf(s),
		rollup: opts.subagentCost && opts.subagentCost.count > 0 ? opts.subagentCost : null,
		pendingCount: opts.pendingCount,
		unreadCount: opts.unreadCount,
		draft: opts.draft,
		draftLaunching: opts.draftLaunching
	};
}
