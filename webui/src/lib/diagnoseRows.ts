import type { SessionListItem } from '@bindings/SessionListItem';
import type { SessionDiagnoseResponse } from '@bindings/SessionDiagnoseResponse';
import type { CodexDiagnose } from '@bindings/CodexDiagnose';
import { sessionEnd } from '$lib/sessionEnd';
import { fmtAge, silenceReasons } from '$lib/diagnoseSilence';
import { m } from '$lib/paraglide/messages';

export type DiagnoseStatus = 'ok' | 'warn' | 'error';
export type DiagnoseBlock = 'process' | 'transport' | 'account';

export type DiagnoseRow = {
	block: DiagnoseBlock;
	label: string;
	status: DiagnoseStatus;
	short: string;
	detail?: string;
};

export type DiagnoseBlockSummary = {
	block: DiagnoseBlock;
	title: string;
	status: DiagnoseStatus;
	short: string;
	rows: DiagnoseRow[];
};

export const DIAGNOSE_BLOCKS: readonly DiagnoseBlock[] = ['process', 'transport', 'account'];

const RANK: Record<DiagnoseStatus, number> = { ok: 0, warn: 1, error: 2 };

export function worstStatus(statuses: DiagnoseStatus[]): DiagnoseStatus {
	return statuses.reduce<DiagnoseStatus>((w, s) => (RANK[s] > RANK[w] ? s : w), 'ok');
}

export function statusDotClass(status: DiagnoseStatus): string {
	return status === 'ok' ? 'dot-active' : status === 'warn' ? 'dot-stale' : 'dot-hibernated';
}

export function blockTitle(block: DiagnoseBlock): string {
	if (block === 'process') return m.diagnose_block_process();
	if (block === 'transport') return m.diagnose_block_transport();
	return m.diagnose_block_account();
}

const TRANSPORT_END_REASONS = new Set(['daemon_lost', 'machine_offline']);

function joinTail(lines: string[] | undefined): string | undefined {
	return lines?.length ? lines.join('\n') : undefined;
}

function processRows(session: SessionListItem | null, report: SessionDiagnoseResponse | null): DiagnoseRow[] {
	const rows: DiagnoseRow[] = [];
	const cx = report?.daemon?.codex ?? null;
	const end = session ? sessionEnd(session) : null;
	if (session) {
		if (end && !TRANSPORT_END_REASONS.has(end.reason)) {
			rows.push({
				block: 'process',
				label: m.diagnose_row_alive(),
				status: end.tone === 'danger' ? 'error' : 'ok',
				short: end.label,
				detail: end.detail ?? undefined
			});
		} else if (end) {
			rows.push({ block: 'process', label: m.diagnose_row_alive(), status: 'warn', short: m.diagnose_unknown() });
		} else {
			const [status, short]: [DiagnoseStatus, string] = session.hibernated
				? ['ok', m.diagnose_short_hibernated()]
				: session.liveness === 'active'
					? ['ok', m.diagnose_short_alive()]
					: session.liveness === 'stale'
						? ['warn', m.diagnose_short_quiet()]
						: ['warn', m.diagnose_short_no_heartbeat()];
			rows.push({ block: 'process', label: m.diagnose_row_alive(), status, short });
		}
	}
	const state = report?.daemon?.effective_state;
	if (state && !cx) {
		rows.push(
			state.value
				? {
						block: 'process',
						label: m.diagnose_fact_effective_state(),
						status: 'ok',
						short: state.value.state ?? state.value.verdict,
						detail: state.value.detail ?? undefined
					}
				: {
						block: 'process',
						label: m.diagnose_fact_effective_state(),
						status: 'warn',
						short: m.diagnose_missing(),
						detail: state.missing_reason ?? undefined
					}
		);
	}
	if (cx) rows.push(codexProcessRow(cx, report!.daemon!.generated_at_ms));
	return rows;
}

function codexProcessRow(cx: CodexDiagnose, generatedAtMs: number): DiagnoseRow {
	const stderr = joinTail(
		(cx.stderr_tail ?? []).map((l) => `${fmtAge(generatedAtMs - l.ts_ms)}  ${l.line}`)
	);
	const pid = cx.app_server_pid != null ? `pid ${cx.app_server_pid}` : m.diagnose_short_down();
	const outdated = cx.version_supported === false;
	return {
		block: 'process',
		label: m.diagnose_codex_app_server(),
		status: !cx.live ? 'error' : outdated ? 'warn' : 'ok',
		short: cx.live ? pid : m.diagnose_short_down(),
		detail: stderr
	};
}

function transportRows(session: SessionListItem | null, report: SessionDiagnoseResponse | null, now: number): DiagnoseRow[] {
	const rows: DiagnoseRow[] = [];
	const end = session ? sessionEnd(session) : null;
	const daemon = report?.daemon ?? null;
	const cx = daemon?.codex ?? null;
	const stderr = cx ? joinTail((cx.stderr_tail ?? []).map((l) => l.line)) : undefined;
	if (end && TRANSPORT_END_REASONS.has(end.reason)) {
		rows.push({
			block: 'transport',
			label: m.diagnose_row_daemon(),
			status: 'error',
			short: end.label,
			detail: [end.detail, stderr].filter(Boolean).join('\n') || undefined
		});
	} else if (report?.daemon_error) {
		rows.push({
			block: 'transport',
			label: m.diagnose_row_daemon(),
			status: 'error',
			short: m.diagnose_short_unreachable(),
			detail: report.daemon_error
		});
	} else if (report || session?.last_heartbeat) {
		const seen = report?.server.machine_last_seen_ms ?? null;
		rows.push({
			block: 'transport',
			label: m.diagnose_row_daemon(),
			status: 'ok',
			short:
				seen != null
					? m.diagnose_daemon_heartbeat({ age: fmtAge(now - seen) })
					: m.diagnose_short_online()
		});
	}
	if (!daemon) return rows;
	if (cx) {
		const reasons = silenceReasons(cx, daemon.generated_at_ms);
		rows.push({
			block: 'transport',
			label: m.diagnose_codex_silence(),
			status: reasons.length && cx.active_turn_id ? 'warn' : 'ok',
			short: reasons.length ? reasons[0] : m.diagnose_short_live(),
			detail: reasons.length > 1 ? reasons.join('\n') : undefined
		});
		return rows;
	}
	const socket = daemon.claude_socket;
	rows.push({
		block: 'transport',
		label: m.diagnose_fact_claude_socket(),
		status: socket.value?.live ? 'ok' : 'warn',
		short: socket.value?.live ? m.diagnose_short_live() : m.diagnose_short_down(),
		detail: socket.value?.path ?? socket.missing_reason ?? undefined
	});
	const attach = daemon.attach.value;
	rows.push({
		block: 'transport',
		label: m.diagnose_fact_attach(),
		status: attach && attach.last_probe_alive !== false ? 'ok' : 'warn',
		short: attach?.phase ?? m.diagnose_missing(),
		detail: attach ? undefined : (daemon.attach.missing_reason ?? undefined)
	});
	const hook = daemon.last_hook_event;
	rows.push({
		block: 'transport',
		label: m.diagnose_row_last_event(),
		status: 'ok',
		short: hook.value ? `${hook.value.kind} · ${fmtAge(hook.age_ms ?? null)}` : fmtAge(hook.age_ms ?? null)
	});
	return rows;
}

function accountRows(session: SessionListItem | null, report: SessionDiagnoseResponse | null): DiagnoseRow[] {
	const rows: DiagnoseRow[] = [];
	const name = session?.account_name ?? report?.server.accounts[0] ?? null;
	if (session || report) {
		const bound = report ? report.server.account_bound : !!session?.has_token_credentials;
		const noTraffic = !!session?.account_name && session.account_traffic_observed === false;
		const status: DiagnoseStatus = !name ? 'ok' : !bound || noTraffic ? 'warn' : 'ok';
		rows.push({
			block: 'account',
			label: m.diagnose_row_account(),
			status,
			short: name ?? m.diagnose_short_ambient(),
			detail: !name
				? undefined
				: !bound
					? m.diagnose_short_unbound()
					: noTraffic
						? m.diagnose_short_no_traffic()
						: undefined
		});
	}
	const gw = report?.daemon?.gateway;
	if (gw?.value && !gw.value.server_configured) {
		rows.push({
			block: 'account',
			label: m.diagnose_fact_gateway(),
			status: 'warn',
			short: m.diagnose_short_not_configured()
		});
	}
	return rows;
}

// `report` is optional: the dot tooltip has only the session.
export function diagnoseRows(
	session: SessionListItem | null,
	report: SessionDiagnoseResponse | null = null,
	now: number = Date.now()
): DiagnoseRow[] {
	return [
		...processRows(session, report),
		...transportRows(session, report, now),
		...accountRows(session, report)
	];
}

/** First and last line of a detail: an end reason keeps the last stderr line next to it. */
export function trimDetail(detail: string | undefined): string {
	const lines = (detail ?? '')
		.split('\n')
		.map((l) => l.trim())
		.filter(Boolean);
	if (lines.length <= 2) return lines.join('\n');
	return `${lines[0]}\n${lines[lines.length - 1]}`;
}

export function diagnoseBlocks(rows: DiagnoseRow[]): DiagnoseBlockSummary[] {
	return DIAGNOSE_BLOCKS.map((block) => {
		const own = rows.filter((r) => r.block === block);
		const status = worstStatus(own.map((r) => r.status));
		const worst = own.find((r) => r.status === status);
		return {
			block,
			title: blockTitle(block),
			status,
			short: !worst ? '—' : status === 'ok' ? worst.short : `${worst.label}: ${worst.short}`,
			rows: own
		};
	});
}
