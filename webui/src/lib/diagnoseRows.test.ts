import { describe, expect, it } from 'vitest';
import type { SessionListItem } from '@bindings/SessionListItem';
import type { SessionDiagnoseResponse } from '@bindings/SessionDiagnoseResponse';
import type { SessionDiagnose } from '@bindings/SessionDiagnose';
import type { DiagnoseFact } from '@bindings/DiagnoseFact';
import type { CodexDiagnose } from '@bindings/CodexDiagnose';
import { endReasonLabel } from './sessionEnd';
import {
	diagnoseBlocks,
	diagnoseRows,
	statusDotClass,
	trimDetail,
	worstStatus,
	type DiagnoseBlock
} from './diagnoseRows';
import blocksSource from './components/molecules/DiagnoseBlocks.svelte?raw';
import dotSource from './components/molecules/SessionDot.svelte?raw';
import factsSource from './components/molecules/SessionDotFacts.svelte?raw';

const NOW = 1_700_000_000_000;

function session(over: Partial<SessionListItem> = {}): SessionListItem {
	return {
		id: 's1',
		parent_id: null,
		machine_id: 'm1',
		working_dir: '/w',
		status: 'active',
		liveness: 'active',
		bucket: 'working',
		metadata: {},
		adapter_id: 'claude_code',
		auto_approve: false,
		cache_cold: false,
		hibernated: false,
		pinned: false,
		labels: [],
		last_heartbeat: new Date(NOW - 5_000).toISOString(),
		account_name: 'team',
		unread_count: 0,
		tool_use_count: 0,
		todos: [],
		has_token_credentials: true,
		account_traffic_observed: true,
		end_reason: null,
		end_detail: null,
		ended_at: null,
		...over
	} as SessionListItem;
}

function fact<T>(value: T | null, over: Partial<DiagnoseFact<T>> = {}): DiagnoseFact<T> {
	return {
		value,
		age_ms: 1_000,
		source: 'hook',
		missing_reason: value === null ? 'not observed' : null,
		...over
	};
}

function daemon(over: Partial<SessionDiagnose> = {}): SessionDiagnose {
	return {
		local_id: 's1',
		short: 'abcd1234',
		generated_at_ms: NOW,
		adapter: 'claude_code',
		effective_state: fact({ verdict: 'hook', state: 'working' }),
		last_hook_event: fact({ kind: 'PostToolUse' }),
		attach: fact({ phase: 'attached', last_probe_alive: true }),
		pty_output: fact({ last_output_age_ms: 500 }),
		claude_socket: fact({ path: '/tmp/s.sock', live: true, candidates: [] }),
		transcript: fact({ path: '/t.jsonl', tail_offset: 0 }),
		prompts: fact({ pending_ask: false, parked_perm_hook: false }),
		permission_mode: fact('default'),
		dispatch: fact({ seen_busy: true, done: false, marker_path: '/m' }),
		gateway: fact({ server_configured: true }),
		...over
	} as SessionDiagnose;
}

function report(over: Partial<SessionDiagnoseResponse> = {}): SessionDiagnoseResponse {
	return {
		session_id: 's1',
		daemon: daemon(),
		daemon_error: null,
		server: {
			status: 'active',
			adapter_id: 'claude_code',
			account_bound: true,
			accounts: ['team'],
			machine_last_seen_ms: NOW - 2_000
		},
		...over
	};
}

const block = (rows: ReturnType<typeof diagnoseRows>, b: DiagnoseBlock) =>
	diagnoseBlocks(rows).find((x) => x.block === b)!;

describe('diagnoseRows: healthy session', () => {
	it('gives the tooltip three green blocks from the session alone', () => {
		const blocks = diagnoseBlocks(diagnoseRows(session(), null, NOW));
		expect(blocks.map((b) => b.block)).toEqual(['process', 'transport', 'account']);
		expect(blocks.map((b) => b.status)).toEqual(['ok', 'ok', 'ok']);
	});

	it('keeps the tooltip green and short with the daemon report', () => {
		const rows = diagnoseRows(session(), report(), NOW);
		expect(rows.every((r) => r.status === 'ok')).toBe(true);
		expect(diagnoseBlocks(rows).every((b) => b.status === 'ok')).toBe(true);
		expect(rows.length).toBeLessThan(15);
	});

	it('describes every row as block, label, status, short', () => {
		for (const r of diagnoseRows(session(), report(), NOW)) {
			expect(['process', 'transport', 'account']).toContain(r.block);
			expect(r.label).toBeTruthy();
			expect(r.short).toBeTruthy();
		}
	});
});

describe('diagnoseRows: dead daemon', () => {
	const dead = session({
		liveness: 'dead',
		end_reason: 'daemon_lost',
		end_detail: 'daemon stderr: panicked at attach.rs',
		ended_at: new Date(NOW - 60_000).toISOString()
	});

	it('turns the transport block red with the end reason and its stderr tail', () => {
		const t = block(diagnoseRows(dead, null, NOW), 'transport');
		expect(t.status).toBe('error');
		const row = t.rows.find((r) => r.status === 'error')!;
		expect(row.short).toBe(endReasonLabel('daemon_lost'));
		expect(row.detail).toContain('panicked at attach.rs');
	});

	it('leaves the process state unknown rather than green', () => {
		expect(block(diagnoseRows(dead, null, NOW), 'process').status).toBe('warn');
	});

	it('gives the tooltip a red transport dot carrying the reason', () => {
		const t = block(diagnoseRows(dead, null, NOW), 'transport');
		expect(statusDotClass(t.status)).toBe('dot-hibernated');
		expect(t.short).toContain(endReasonLabel('daemon_lost'));
	});

	it('appends the codex stderr tail when the report has one', () => {
		const cx = {
			min_version: '0.1.0',
			transport: 'stdio',
			live: false,
			registered: true,
			turn_status: 'idle',
			pending_rpc_count: 0,
			pending_rpc_methods: [],
			stderr_tail: [{ ts_ms: NOW - 100, line: 'app-server exited 1' }]
		} as unknown as CodexDiagnose;
		const t = block(diagnoseRows(dead, report({ daemon: daemon({ codex: cx }) }), NOW), 'transport');
		expect(t.rows[0].detail).toContain('app-server exited 1');
	});

	it('marks an unreachable daemon red even before the session ends', () => {
		const t = block(
			diagnoseRows(session(), report({ daemon: null, daemon_error: 'timeout' }), NOW),
			'transport'
		);
		expect(t.status).toBe('error');
		expect(t.rows[0].detail).toBe('timeout');
	});
});

describe('diagnoseRows: other failures', () => {
	it('puts a crash in the process block', () => {
		const p = block(diagnoseRows(session({ end_reason: 'crashed', end_detail: 'boom' }), null, NOW), 'process');
		expect(p.status).toBe('error');
		expect(p.rows[0].detail).toBe('boom');
	});

	it('flags a quiet session orange', () => {
		expect(block(diagnoseRows(session({ liveness: 'stale' }), null, NOW), 'process').status).toBe('warn');
	});

	it('flags an account whose gateway never saw traffic', () => {
		const a = block(diagnoseRows(session({ account_traffic_observed: false }), null, NOW), 'account');
		expect(a.status).toBe('warn');
	});

	it('flags a dead claude socket orange', () => {
		const d = daemon({ claude_socket: fact({ path: null, live: false, candidates: [] }) });
		expect(block(diagnoseRows(session(), report({ daemon: d }), NOW), 'transport').status).toBe('warn');
	});
});

describe('status helpers', () => {
	it('rolls a block up to its worst child', () => {
		expect(worstStatus([])).toBe('ok');
		expect(worstStatus(['ok', 'warn'])).toBe('warn');
		expect(worstStatus(['warn', 'error', 'ok'])).toBe('error');
	});

	it('maps statuses onto the liveness dot tones', () => {
		expect(statusDotClass('ok')).toBe('dot-active');
		expect(statusDotClass('warn')).toBe('dot-stale');
		expect(statusDotClass('error')).toBe('dot-hibernated');
	});
});

describe('trimDetail', () => {
	it('keeps one or two lines as they are', () => {
		expect(trimDetail('boom')).toBe('boom');
		expect(trimDetail('boom\npanicked')).toBe('boom\npanicked');
	});

	it('keeps the end reason and the last stderr line of a long tail', () => {
		expect(trimDetail('daemon stderr:\nfirst\nsecond\nlast line')).toBe('daemon stderr:\nlast line');
	});

	it('is empty for a missing detail', () => {
		expect(trimDetail(undefined)).toBe('');
	});
});

describe('the dot tooltip is the only diagnose surface', () => {
	it('builds its blocks from diagnoseRows and the lazily fetched report', () => {
		expect(factsSource).toContain('diagnoseRows(');
		expect(factsSource).toContain('useSessionDiagnose(');
		expect(dotSource).toContain('armed = true');
	});

	it('keeps the query out of the dot itself: no observer until the tooltip is armed', () => {
		expect(dotSource).not.toContain('useSessionDiagnose');
		expect(dotSource).toContain('{#if armed}');
	});

	it('has no link to a diagnose panel', () => {
		expect(dotSource).not.toContain('diagnose=1');
		expect(dotSource).not.toContain('DiagnosePanel');
	});

	it('trims non-green details and offers the full text on the clipboard', () => {
		expect(blocksSource).toContain('trimDetail(r.detail)');
		expect(blocksSource).toContain('<CopyButton');
		expect(blocksSource).toContain("b.status !== 'ok'");
	});

	it('adds no :global override', () => {
		for (const src of [blocksSource, dotSource, factsSource]) expect(src).not.toContain(':global(');
	});
});
