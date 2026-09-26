import { describe, expect, it } from 'vitest';
import type { AgentEvent } from '@bindings/AgentEvent';
import { allFilter, defaultFilter } from './filters';
import { latestTodoLineKey } from './format';
import { buildLines, type LineBuildCtx } from './lines';
import type { MsgCategory } from './types';

const ctx = (overrides: Partial<Record<MsgCategory, boolean>> = {}): LineBuildCtx => {
	const filter = { ...allFilter(true), ...overrides };
	return {
		visible: (c) => filter[c],
		renderMarkdown: (s) => `<p>${s}</p>`,
		renderCode: (text) => `<code>${text}</code>`,
		prettyJson: true,
		prettyDiff: true
	};
};

const only = (...cats: MsgCategory[]): LineBuildCtx => {
	const filter = allFilter(false);
	for (const c of cats) filter[c] = true;
	return { ...ctx(), visible: (c) => filter[c] };
};

const text = (
	content: string,
	ts: number,
	kind: string | null = null,
	seq: number | null = null
): AgentEvent => ({
	type: 'text',
	content,
	meta: false,
	kind,
	ts,
	message_id: null,
	usage: null,
	seq
});

const summary = (
	detail: string,
	ts: number,
	opts: { needsAction?: boolean; category?: string | null } = {}
): AgentEvent => ({
	type: 'turn_summary',
	detail,
	status_category: opts.category ?? null,
	needs_action: opts.needsAction ?? false,
	ts,
	seq: null
});

const roles = (es: AgentEvent[], c = ctx()) => buildLines(es, c).map((l) => l.role);

const queueOp = (
	operation: 'queued' | 'dequeued' | 'removed' | 'cleared' | 'absorbed',
	body: string,
	ts: number,
	seq: number | null = null
): AgentEvent => ({
	...(text(body, ts, 'queue_op', seq) as AgentEvent & { type: 'text' }),
	operation
});

const toolCall = (tool: string, ts: number, kind: string | null = null): AgentEvent => ({
	type: 'tool_call',
	tool,
	input: {},
	kind,
	ts,
	seq: null
});

const toolResult = (
	ts: number,
	opts: { kind?: string | null; error?: boolean; output?: string } = {}
): AgentEvent => ({
	type: 'tool_result',
	tool: 'Bash',
	output_summary: opts.output ?? 'ok',
	kind: opts.kind ?? null,
	error: opts.error ?? false,
	ts,
	seq: null
});

describe('per-category visibility', () => {
	const events: AgentEvent[] = [
		text('▷ User: go', 1),
		text('▷ User: <system-reminder>be brief</system-reminder>', 2),
		text('thought', 3, 'thinking'),
		text('[redacted thinking]', 4, 'redacted_thinking'),
		text('answer', 5),
		text('[image attachment]', 6, 'attachment'),
		text('· permission mode: plan', 7, 'system_marker'),
		toolCall('Bash', 8),
		toolCall('mcp__pg__query', 9),
		toolResult(10),
		{ type: 'compact_summary', content: 'so far…', ts: 11, seq: null },
		{ type: 'context_reset', ts: 12, seq: null },
		summary('wrapped up', 13),
		toolCall('web_search', 14, 'server_tool_use'),
		toolResult(15, { kind: 'server_tool_result', output: 'search hits' }),
		toolResult(16, { error: true, output: 'boom' })
	];

	const cases: [MsgCategory, string, number][] = [
		['user', 'user', 1],
		['system', 'system', 2],
		['thinking', 'thinking', 3],
		['redacted', 'thinking', 4],
		['assistant', 'assistant', 5],
		['attachment', 'assistant', 6],
		['marker', 'marker', 7],
		['tool', 'tool', 8],
		['mcp', 'tool', 9],
		['result', 'result', 10],
		['compact', 'compact', 11],
		['reset', 'reset', 12],
		['summary', 'summary', 13],
		['server_tool', 'tool', 14],
		['server_result', 'result', 15],
		['error', 'result', 16]
	];

	it.each(cases)('renders only its own line when %s is the sole category', (cat, role, ts) => {
		const shown = buildLines(events, only(cat));
		expect(shown.map((l) => [l.role, l.ts])).toEqual([[role, ts]]);
	});

	it.each(cases)('drops its own line, and only that one, when %s is off', (cat, _role, ts) => {
		const kept = buildLines(events, ctx({ [cat]: false }));
		expect(kept.map((l) => l.ts)).not.toContain(ts);
		for (const [, , otherTs] of cases) {
			// A summary with no assistant bubble left to hang on is a line of its
			// own; once one exists again it becomes that bubble's footer.
			if (otherTs === ts || otherTs === 13) continue;
			expect(kept.map((l) => l.ts)).toContain(otherTs);
		}
	});

	it('renders every category when nothing is filtered', () => {
		expect(roles(events)).toEqual([
			'user',
			'system',
			'thinking',
			'thinking',
			'assistant',
			'assistant',
			'marker',
			'tool',
			'tool',
			'result',
			'compact',
			'reset',
			'summary',
			'tool',
			'result',
			'result'
		]);
	});

	it('classes an errored server result under error, not server_result', () => {
		const events = [toolResult(1, { kind: 'server_tool_result', error: true })];
		expect(roles(events, only('server_result'))).toEqual([]);
		expect(roles(events, only('error'))).toEqual(['result']);
	});
});

describe('thinking lines', () => {
	it('maps kind "thinking" to its own role, not assistant', () => {
		const lines = buildLines([text('weighing options', 1, 'thinking')], ctx());
		expect(lines).toHaveLength(1);
		expect(lines[0].role).toBe('thinking');
		expect(lines[0].text).toBe('weighing options');
		expect(lines[0].redacted).toBe(false);
	});

	it('maps kind "redacted_thinking" to a thinking line flagged redacted', () => {
		const lines = buildLines([text('[redacted thinking]', 1, 'redacted_thinking')], ctx());
		expect(lines[0].role).toBe('thinking');
		expect(lines[0].redacted).toBe(true);
	});

	it('leaves plain text (kind null) an ordinary assistant line', () => {
		const lines = buildLines([text('here is the answer', 1)], ctx());
		expect(lines[0].role).toBe('assistant');
		expect(lines[0].redacted).toBeUndefined();
	});

	it('keeps attachments assistant-side and markers on their own role', () => {
		expect(roles([text('a file', 1, 'attachment')])).toEqual(['assistant']);
		expect(roles([text('· agent name: qa', 1, 'system_marker')])).toEqual(['marker']);
	});

	it('filters redacted thinking apart from ordinary thinking', () => {
		const events = [
			text('thought', 1, 'thinking'),
			text('[redacted thinking]', 2, 'redacted_thinking')
		];
		expect(buildLines(events, ctx({ redacted: false })).map((l) => l.text)).toEqual(['thought']);
		expect(buildLines(events, ctx({ thinking: false })).map((l) => l.text)).toEqual([
			'[redacted thinking]'
		]);
	});

	it('is shown by default and hidden when switched off', () => {
		const events = [text('thought', 1, 'thinking'), text('answer', 2)];
		expect(roles(events)).toEqual(['thinking', 'assistant']);
		expect(roles(events, ctx({ thinking: false }))).toEqual(['assistant']);
	});

	it('is the only role left when everything else is off', () => {
		const events = [text('thought', 1, 'thinking'), text('answer', 2)];
		expect(roles(events, only('thinking'))).toEqual(['thinking']);
	});

	it('does not take a turn number or a fork anchor', () => {
		const lines = buildLines([text('▷ User: go', 1), text('thought', 2, 'thinking')], ctx());
		expect(lines[1].turn).toBeUndefined();
		expect(lines[1].messageId).toBeUndefined();
	});
});

describe('turn summaries', () => {
	it('attaches to the preceding assistant line instead of adding one', () => {
		const lines = buildLines([text('done', 1), summary('Refactored the parser', 2)], ctx());
		expect(lines).toHaveLength(1);
		expect(lines[0].role).toBe('assistant');
		expect(lines[0].summary).toEqual({
			detail: 'Refactored the parser',
			needsAction: false,
			ts: 2
		});
	});

	it('carries needs_action through', () => {
		const lines = buildLines(
			[text('done', 1), summary('Waiting on a decision', 2, { needsAction: true })],
			ctx()
		);
		expect(lines[0].summary?.needsAction).toBe(true);
	});

	it('falls back to status_category when detail is empty', () => {
		const lines = buildLines([text('done', 1), summary('  ', 2, { category: 'blocked' })], ctx());
		expect(lines[0].summary?.detail).toBe('blocked');
	});

	it('drops a summary with neither detail nor category', () => {
		const lines = buildLines([text('done', 1), summary('', 2, { category: '  ' })], ctx());
		expect(lines).toHaveLength(1);
		expect(lines[0].summary).toBeUndefined();
	});

	it('skips back over tool lines to the turn’s last assistant bubble', () => {
		const toolCall: AgentEvent = { type: 'tool_call', tool: 'Bash', input: {}, ts: 2, seq: null };
		const toolResult: AgentEvent = {
			type: 'tool_result',
			tool: 'Bash',
			output_summary: 'ok',
			error: false,
			ts: 3,
			seq: null
		};
		const lines = buildLines(
			[text('running it', 1), toolCall, toolResult, summary('Ran the suite', 4)],
			ctx()
		);
		expect(lines.map((l) => l.role)).toEqual(['assistant', 'tool', 'result']);
		expect(lines[0].summary?.detail).toBe('Ran the suite');
	});

	it('stands alone when no assistant line opened the turn', () => {
		const lines = buildLines([text('▷ User: go', 1), summary('Nothing to do', 2)], ctx());
		expect(lines.map((l) => l.role)).toEqual(['user', 'summary']);
		expect(lines[1].summary?.detail).toBe('Nothing to do');
	});

	it('stands alone rather than overwriting an assistant line that already has one', () => {
		const lines = buildLines(
			[text('done', 1), summary('first', 2), summary('second', 3)],
			ctx()
		);
		expect(lines.map((l) => l.role)).toEqual(['assistant', 'summary']);
		expect(lines[0].summary?.detail).toBe('first');
		expect(lines[1].summary?.detail).toBe('second');
	});

	it('is hidden when switched off, leaving the assistant bubble untouched', () => {
		const events = [text('done', 1), summary('Refactored the parser', 2)];
		const lines = buildLines(events, ctx({ summary: false }));
		expect(lines).toHaveLength(1);
		expect(lines[0].summary).toBeUndefined();
	});

	it('survives an assistant line hidden by the filter, as a standalone footer', () => {
		const events = [text('done', 1), summary('Refactored the parser', 2)];
		const lines = buildLines(events, ctx({ assistant: false }));
		expect(lines.map((l) => l.role)).toEqual(['summary']);
	});

	it('stays invisible to the consecutive-duplicate guard when attached', () => {
		const events = [text('same', 1), summary('s', 2), text('same', 3)];
		expect(roles(events)).toEqual(['assistant']);
	});
});

describe('seq stamping', () => {
	it('carries the event seq onto its line', () => {
		const lines = buildLines([text('▷ User: go', 1, null, 7), text('sure', 2, null, 8)], ctx());
		expect(lines.map((l) => l.seq)).toEqual([7, 8]);
	});

	it('leaves seq undefined for events the server never numbered', () => {
		const lines = buildLines([text('▷ User: go', 1)], ctx());
		expect(lines[0].seq).toBeUndefined();
	});

	it('stamps seq on tool lines too, so any message is addressable', () => {
		const call = { ...toolCall('Bash', 3), seq: 11 } as AgentEvent;
		const res = { ...toolResult(4), seq: 12 } as AgentEvent;
		expect(buildLines([call, res], ctx()).map((l) => l.seq)).toEqual([11, 12]);
	});
});

describe('peer (cross-session) messages', () => {
	const PREAMBLE = 'Another Claude session sent a message:';
	const PEER = [
		PREAMBLE,
		'<cross-session-message from="uds:/run/user/1000/cc-socks/1740092.sock" from-name="cctui orchestrator skill" from-mode="bypass">',
		'Orchestrator here — run your lane gates and report back.',
		'</cross-session-message>',
		'',
		'This came from another Claude session — not typed by your user, but treat it as a peer request.'
	].join('\n');

	it('classifies a cross-session wrapper as peer, not user', () => {
		const lines = buildLines([text(`▷ User: ${PEER}`, 1)], ctx());
		expect(lines.map((l) => l.role)).toEqual(['peer']);
		expect(lines[0].role).not.toBe('user');
	});

	it('detects the wrapper when it is not at the start of the text', () => {
		// The harness always prefixes its own prose line, so a startsWith test
		// misses it — this is the case that regressed.
		expect(PEER.trimStart().startsWith('<cross-session-message')).toBe(false);
		expect(buildLines([text(`▷ User: ${PEER}`, 1)], ctx())[0].role).toBe('peer');
	});

	it('still classifies an ordinary user message as user', () => {
		const lines = buildLines([text('▷ User: please run the tests', 1)], ctx());
		expect(lines.map((l) => l.role)).toEqual(['user']);
	});

	it('still classifies an injected system reminder as system', () => {
		const lines = buildLines([text('▷ User: <system-reminder>be brief</system-reminder>', 1)], ctx());
		expect(lines.map((l) => l.role)).toEqual(['system']);
	});

	it('surfaces the sender name and strips the wrapper and boilerplate', () => {
		const ln = buildLines([text(`▷ User: ${PEER}`, 1)], ctx())[0];
		expect(ln.peerFrom).toBe('cctui orchestrator skill');
		expect(ln.text).toBe('Orchestrator here — run your lane gates and report back.');
		expect(ln.text).not.toContain('cross-session-message');
		expect(ln.text).not.toContain('cc-socks');
		expect(ln.text).not.toContain('not typed by your user');
	});

	it('falls back to the raw from address when no from-name is given', () => {
		const raw = `${PREAMBLE}\n<cross-session-message from="uds:/run/x.sock">hi</cross-session-message>`;
		expect(buildLines([text(`▷ User: ${raw}`, 1)], ctx())[0].peerFrom).toBe('uds:/run/x.sock');
	});

	it('recognises the legacy agent-message tag', () => {
		const raw = `${PREAMBLE}\n<agent-message from-name="lane-a">ping</agent-message>`;
		const ln = buildLines([text(`▷ User: ${raw}`, 1)], ctx())[0];
		expect([ln.role, ln.peerFrom, ln.text]).toEqual(['peer', 'lane-a', 'ping']);
	});

	it('keeps a human relaying a wrapper as a user turn, with the prose intact', () => {
		const raw = 'forward this: <cross-session-message from="x">hi</cross-session-message>';
		const ln = buildLines([text(`▷ User: ${raw}`, 1)], ctx())[0];
		expect(ln.role).toBe('user');
		expect(ln.text).toBe(raw);
		expect(ln.peerFrom).toBeUndefined();
	});

	it('keeps a human turn whose prose precedes a wrapper on its own line as user', () => {
		const raw = 'peer says:\n<cross-session-message from="uds:/run/x.sock">hi</cross-session-message>';
		const ln = buildLines([text(`▷ User: ${raw}`, 1)], ctx())[0];
		expect(ln.role).toBe('user');
		expect(ln.text).toBe(raw);
	});

	it('accepts the wrapper when only the harness preamble precedes it', () => {
		const raw = `${PREAMBLE}\n<cross-session-message from-name="lane-a">hi</cross-session-message>`;
		const ln = buildLines([text(`▷ User: ${raw}`, 1)], ctx())[0];
		expect([ln.role, ln.peerFrom, ln.text]).toEqual(['peer', 'lane-a', 'hi']);
	});

	it('is filterable on its own category, independently of user', () => {
		const events = [text('▷ User: typed', 1), text(`▷ User: ${PEER}`, 2)];
		expect(buildLines(events, ctx({ peer: false })).map((l) => l.role)).toEqual(['user']);
		expect(buildLines(events, ctx({ user: false })).map((l) => l.role)).toEqual(['peer']);
	});
});

describe('poll re-injection classification', () => {
	const POLL = 'Check the queue depth and report anything above 100. Do not stop.';
	const typed = (body: string, ts: number, turnId: string): AgentEvent => ({
		...(text(`▷ User: ${body}`, ts) as AgentEvent & { type: 'text' }),
		turn_id: turnId
	});

	it('keeps the first occurrence user and tints every repeat', () => {
		const events = [text(`▷ User: ${POLL}`, 1), text(`▷ User: ${POLL}`, 2)];
		expect(roles(events)).toEqual(['user', 'poll']);
	});

	it('leaves a repeat separated by an assistant turn alone', () => {
		const events = [
			text(`▷ User: ${POLL}`, 1),
			text('nothing above 100', 2),
			text(`▷ User: ${POLL}`, 3),
			text('still nothing', 4),
			text(`▷ User: ${POLL}`, 5)
		];
		expect(roles(events)).toEqual(['user', 'assistant', 'user', 'assistant', 'user']);
	});

	it('never demotes a human repeating themselves: a composer send carries a turn_id', () => {
		const events = [typed('continue', 1, 'a'), text('working', 2), typed('continue', 3, 'b')];
		expect(roles(events)).toEqual(['user', 'assistant', 'user']);
	});

	it('does not demote two consecutive composer sends of the same text', () => {
		expect(roles([typed('continue', 1, 'a'), typed('continue', 2, 'b')])).toEqual([
			'user',
			'user'
		]);
	});

	it('renders two consecutive composer sends of the same text as two bubbles', () => {
		const lines = buildLines([typed('continue', 1, 'a'), typed('continue', 2, 'b')], ctx());
		expect(lines).toHaveLength(2);
		expect(lines.map((l) => l.ts)).toEqual([1, 2]);
	});

	it('still collapses the encodings of ONE turn, which share its turn_id', () => {
		const lines = buildLines([typed('continue', 1, 'a'), typed('continue', 2, 'a')], ctx());
		expect(lines).toHaveLength(1);
	});

	it('still tints a consecutive re-injection that carries no turn_id', () => {
		const events = [text(`▷ User: ${POLL}`, 1), text(`▷ User: ${POLL}`, 2)];
		expect(roles(events)).toEqual(['user', 'poll']);
	});

	it('closes the run on a tool call, so a repeat after real work stays user', () => {
		const events = [
			text(`▷ User: ${POLL}`, 1),
			toolCall('Bash', 2),
			text(`▷ User: ${POLL}`, 3)
		];
		expect(roles(events)).toEqual(['user', 'tool', 'user']);
	});

	it('ignores whitespace differences when matching a repeat', () => {
		const events = [text('▷ User: run  the\nsweep', 1), text('▷ User: run the sweep', 2)];
		expect(roles(events)).toEqual(['user', 'poll']);
	});

	it('does not tint near-identical but different prose', () => {
		const events = [
			text('▷ User: check the queue depth', 1),
			text('▷ User: check the queue depths', 2)
		];
		expect(roles(events)).toEqual(['user', 'user']);
	});

	it('tints a marker-tagged injection on its first occurrence', () => {
		expect(roles([text('▷ User: # Autonomous loop\n\ndo the thing', 1)])).toEqual(['poll']);
	});

	it('tints the scheduler wake-up sentinels', () => {
		expect(roles([text('▷ User: <<autonomous-loop-dynamic>>', 1)])).toEqual(['poll']);
	});

	it('hides poll noise without hiding the human turn it repeats', () => {
		const events = [text(`▷ User: ${POLL}`, 1), text(`▷ User: ${POLL}`, 2)];
		expect(buildLines(events, ctx({ poll: false })).map((l) => l.role)).toEqual(['user']);
	});

	it('still classifies a repeat when the first occurrence is filtered out', () => {
		const events = [text(`▷ User: ${POLL}`, 1), text(`▷ User: ${POLL}`, 2)];
		expect(buildLines(events, ctx({ user: false })).map((l) => l.role)).toEqual(['poll']);
	});

	it('keeps delivery state on a repeat of a still-sending turn', () => {
		const events = [text(`▷ User: ${POLL}`, 1), text(`▷ User: ${POLL}`, 2)];
		const delivery = {
			pending: new Set([2]),
			failed: new Map<number, string>(),
			retrying: new Map<number, { attempt: number; max: number }>()
		};
		const lines = buildLines(events, ctx(), delivery);
		expect(lines[1].role).toBe('poll');
		expect(lines[1].pending).toBe(true);
	});

	it('is visible by default', () => {
		expect(defaultFilter().poll).toBe(true);
	});
});

describe('task lists', () => {
	const todoWrite = (ts: number, ...contents: [string, string][]): AgentEvent => ({
		type: 'tool_call',
		tool: 'TodoWrite',
		input: { todos: contents.map(([content, status]) => ({ content, status, activeForm: `Doing ${content}` })) },
		kind: null,
		ts,
		seq: ts
	});

	it('parses a TodoWrite tool_call into a todo line instead of a JSON bubble', () => {
		const [ln] = buildLines([todoWrite(1, ['a', 'pending'])], ctx());
		expect(ln.todos).toEqual([{ content: 'a', status: 'pending', activeForm: 'Doing a' }]);
		expect(ln.htmlCode).toBeUndefined();
	});

	it('parses a codex update_plan tool_call the same way', () => {
		const ev: AgentEvent = {
			type: 'tool_call',
			tool: 'update_plan',
			input: { plan: [{ step: 'read code', status: 'in_progress' }] },
			kind: null,
			ts: 1,
			seq: 1
		};
		expect(buildLines([ev], ctx())[0].todos?.[0]).toEqual({
			content: 'read code',
			status: 'in_progress',
			activeForm: undefined
		});
	});

	it('folds three successive TodoWrite calls to exactly one rendered card carrying the newest array', () => {
		const lines = buildLines(
			[
				todoWrite(1, ['a', 'pending'], ['b', 'pending']),
				todoWrite(2, ['a', 'completed'], ['b', 'pending']),
				todoWrite(3, ['a', 'completed'], ['b', 'in_progress'])
			],
			ctx()
		);
		const todoLines = lines.filter((l) => l.todos);
		expect(todoLines).toHaveLength(3);

		const latest = latestTodoLineKey(lines);
		const rendered = todoLines.filter((l) => l.key === latest);
		expect(rendered).toHaveLength(1);
		expect(rendered[0].todos).toEqual([
			{ content: 'a', status: 'completed', activeForm: 'Doing a' },
			{ content: 'b', status: 'in_progress', activeForm: 'Doing b' }
		]);
	});

	it('renders the NEWEST list, not the stalest, across successive updates', () => {
		const lines = buildLines(
			[todoWrite(1, ['a', 'pending']), todoWrite(2, ['a', 'in_progress']), todoWrite(3, ['a', 'completed'])],
			ctx()
		);
		const rendered = lines.filter((l) => l.todos).find((l) => l.key === latestTodoLineKey(lines));
		expect(rendered?.todos?.[0].status).toBe('completed');
	});

	it('still collapses two IDENTICAL consecutive task lists, as the dupe guard intends', () => {
		const lines = buildLines([todoWrite(1, ['a', 'pending']), todoWrite(2, ['a', 'pending'])], ctx());
		expect(lines.filter((l) => l.todos)).toHaveLength(1);
	});

	it('leaves a malformed TodoWrite as an ordinary tool bubble', () => {
		const ev: AgentEvent = { type: 'tool_call', tool: 'TodoWrite', input: { todos: [] }, kind: null, ts: 1, seq: 1 };
		const [ln] = buildLines([ev], ctx());
		expect(ln.todos).toBeUndefined();
		expect(ln.role).toBe('tool');
	});
});

describe('CCT-1055 system record reclassification', () => {
	const annotation = (detail: string, ts: number) => text(detail, ts, 'turn_annotation');
	const marker = (body: string, ts: number) => text(`· ${body}`, ts, 'system_marker');
	const user = (body: string, ts: number) => text(`▷ User: ${body}`, ts);

	it('never counts harness attachment records on the user turn they precede', () => {
		const lines = buildLines(
			[
				annotation('attachment:environment', 1),
				annotation('attachment:prompt_snapshot', 2),
				user('do the thing', 3)
			],
			ctx()
		);
		expect(lines.map((l) => l.role)).toEqual(['user']);
		expect(lines[0]).not.toHaveProperty('attachmentCount');
	});

	it('drops attachment annotations that never reach a user turn', () => {
		const lines = buildLines([annotation('attachment:prompt_snapshot', 1)], ctx());
		expect(lines).toHaveLength(0);
	});

	it('never renders a queue operation as a marker row', () => {
		const lines = buildLines([queueOp('queued', 'deploy the thing', 1)], ctx());
		expect(lines.map((l) => l.role)).toEqual(['user']);
		expect(lines[0].text).toBe('deploy the thing');
	});

	it('groups consecutive markers into one row, keeping every text', () => {
		const lines = buildLines([marker('mode: normal', 1), marker('worktree state: clean', 2)], ctx());
		expect(lines).toHaveLength(1);
		expect(lines[0].markerTexts).toEqual(['· mode: normal', '· worktree state: clean']);
	});

	it('takes turn_duration as the exact assistant duration instead of the ts estimate', () => {
		const lines = buildLines(
			[user('go', 1000), text('done', 9000), annotation('turn_duration:1200', 9001)],
			ctx()
		);
		const assistant = lines.find((l) => l.role === 'assistant');
		expect(assistant?.durationMs).toBe(1200);
	});

	it('still estimates an assistant duration when no annotation arrives', () => {
		const lines = buildLines([user('go', 1000), text('done', 9000)], ctx());
		expect(lines.find((l) => l.role === 'assistant')?.durationMs).toBe(8000);
	});

	it('attaches a stop_hook_summary to the assistant turn, not the timeline', () => {
		const lines = buildLines(
			[user('go', 1), text('done', 2), annotation('stop_hook_summary:hook ran', 3)],
			ctx()
		);
		expect(lines.map((l) => l.role)).toEqual(['user', 'assistant']);
		expect(lines[1].stopHook).toBe('hook ran');
	});

	it('attaches file-history provenance to the adjacent tool call', () => {
		const tool: AgentEvent = {
			type: 'tool_call',
			tool: 'Edit',
			input: { file_path: 'src/a.rs' },
			kind: null,
			ts: 1,
			seq: 1
		};
		const lines = buildLines([tool, annotation('file_history:snapshot:src/a.rs', 2)], ctx());
		expect(lines.map((l) => l.role)).toEqual(['tool']);
		expect(lines[0].fileHistory).toEqual(['snapshot:src/a.rs']);
	});
});

describe('CCT-1083 queued messages carry their own queue state', () => {
	const user = (body: string, ts: number, seq: number | null = null) =>
		text(`▷ User: ${body}`, ts, null, seq);

	it('renders a queued prompt with no matching user event as a waiting user bubble', () => {
		const lines = buildLines([queueOp('queued', 'ship the thing', 1, 5)], ctx());
		expect(lines).toHaveLength(1);
		expect(lines[0].role).toBe('user');
		expect(lines[0].queued).toBe(true);
		expect(lines[0].queuedAt).toBeUndefined();
		expect(lines[0].cancelled).toBeUndefined();
	});

	it('collapses a queued prompt and its delivered turn into one bubble', () => {
		const lines = buildLines(
			[queueOp('queued', 'ship the thing', 1, 5), user('ship the thing', 3, 9)],
			ctx()
		);
		expect(lines).toHaveLength(1);
		expect(lines[0].queued).toBeUndefined();
		expect(lines[0].queuedAt).toBe(1);
	});

	it('gives the delivered bubble the real user event seq, never the enqueue row id', () => {
		const lines = buildLines(
			[queueOp('queued', 'ship the thing', 1, 5), user('ship the thing', 3, 9)],
			ctx()
		);
		expect(lines[0].seq).toBe(9);
	});

	it('keeps the delivered bubble when the dequeue op sits between the pair', () => {
		const lines = buildLines(
			[
				queueOp('queued', 'ship the thing', 1, 5),
				queueOp('dequeued', '', 2, 6),
				user('ship the thing', 3, 9)
			],
			ctx()
		);
		expect(lines).toHaveLength(1);
		expect(lines[0].seq).toBe(9);
		expect(lines[0].queued).toBeUndefined();
		expect(lines[0].queuedAt).toBe(1);
		expect(lines[0].cancelled).toBeUndefined();
	});

	it('marks a queued prompt cancelled when it is dequeued with no user event', () => {
		const lines = buildLines(
			[queueOp('queued', 'ship the thing', 1, 5), queueOp('dequeued', '', 2, 6)],
			ctx()
		);
		expect(lines).toHaveLength(1);
		expect(lines[0].cancelled).toBe(true);
		expect(lines[0].text).toBe('ship the thing');
	});

	it('marks a queued prompt cancelled when a remove op names it', () => {
		const lines = buildLines(
			[queueOp('queued', 'ship the thing', 1, 5), queueOp('removed', 'ship the thing', 2, 6)],
			ctx()
		);
		expect(lines[0].cancelled).toBe(true);
	});

	it('correlates a long multi-line prompt through its truncated first line', () => {
		const first = 'a'.repeat(200);
		const full = `${first}\nsecond line`;
		const truncated = `${first.slice(0, 120)}…`;
		const lines = buildLines([queueOp('queued', truncated, 1, 5), user(full, 3, 9)], ctx());
		expect(lines).toHaveLength(1);
		expect(lines[0].seq).toBe(9);
		expect(lines[0].queuedAt).toBe(1);
	});

	it('does not attach a queued prompt to a different message', () => {
		const lines = buildLines(
			[queueOp('queued', 'ship the thing', 1, 5), user('something else entirely', 3, 9)],
			ctx()
		);
		expect(lines).toHaveLength(2);
		expect(lines[0].queued).toBe(true);
		expect(lines[1].queued).toBeUndefined();
	});

	it('resolves two queued prompts to their own delivered turns', () => {
		const lines = buildLines(
			[
				queueOp('queued', 'first', 1, 5),
				queueOp('queued', 'second', 2, 6),
				user('first', 3, 9),
				text('done', 4, null, 10),
				user('second', 5, 11)
			],
			ctx()
		);
		expect(lines.map((l) => [l.text, l.seq, l.queuedAt])).toEqual([
			['first', 9, 1],
			['done', 10, undefined],
			['second', 11, 2]
		]);
	});

	it('follows the user filter, not the marker filter', () => {
		const events = [queueOp('queued', 'ship the thing', 1, 5)];
		expect(buildLines(events, ctx({ marker: false }))).toHaveLength(1);
		expect(buildLines(events, ctx({ user: false }))).toHaveLength(0);
	});

	it('does not let the duplicate guard swallow the delivered turn', () => {
		const lines = buildLines(
			[queueOp('queued', 'same text', 1, 5), user('same text', 2, 6)],
			ctx()
		);
		expect(lines).toHaveLength(1);
		expect(lines[0].seq).toBe(6);
	});
});

describe('CCT-1101 queued means waiting, absorbed means delivered', () => {
	const user = (body: string, ts: number, seq: number | null = null) =>
		text(`▷ User: ${body}`, ts, null, seq);

	it('leaves a dequeued-then-delivered message unlabelled', () => {
		const lines = buildLines(
			[
				queueOp('queued', 'what is using the GPU?', 1, 5),
				queueOp('dequeued', '', 2, 6),
				user('what is using the GPU?', 3, 9)
			],
			ctx()
		);
		expect(lines).toHaveLength(1);
		expect(lines[0].queued).toBeUndefined();
		expect(lines[0].cancelled).toBeUndefined();
		expect(lines[0].queuedAt).toBe(1);
	});

	it('keeps a queued prompt with no close labelled queued', () => {
		const lines = buildLines([queueOp('queued', 'ship the thing', 1, 5)], ctx());
		expect(lines[0].queued).toBe(true);
		expect(lines[0].queuedAt).toBeUndefined();
		expect(lines[0].cancelled).toBeUndefined();
	});

	it('turns an absorbed mid-turn prompt into one delivered line with body and chips', () => {
		const body = [
			'just testing the paste feature',
			'',
			'Attached file:',
			'- /tmp/cctui-uploads/sess/paste-1-2.txt'
		].join('\n');
		const lines = buildLines(
			[
				queueOp('queued', 'just testing the paste feature', 1, 5),
				queueOp('absorbed', 'just testing the paste feature', 2, 6),
				user(body, 3, 9)
			],
			ctx()
		);
		expect(lines).toHaveLength(1);
		expect(lines[0].cancelled).toBeUndefined();
		expect(lines[0].queued).toBeUndefined();
		expect(lines[0].text).toBe('just testing the paste feature');
		expect(lines[0].uploads).toEqual({ sessionId: 'sess', names: ['paste-1-2.txt'] });
	});

	it('does not cancel an absorbed placeholder that has no user event', () => {
		const lines = buildLines(
			[queueOp('queued', 'ship the thing', 1, 5), queueOp('absorbed', 'ship the thing', 2, 6)],
			ctx()
		);
		expect(lines).toHaveLength(1);
		expect(lines[0].cancelled).toBeUndefined();
		expect(lines[0].queuedAt).toBe(1);
	});

	it('matches a placeholder whose first line still carries the paste token', () => {
		const lines = buildLines(
			[
				queueOp('queued', '[paste-1.txt] look at this', 1, 5),
				user('[paste-1.txt] look at this\n\nAttached file:\n- /tmp/cctui-uploads/s/paste-1.txt', 3, 9)
			],
			ctx()
		);
		expect(lines).toHaveLength(1);
		expect(lines[0].seq).toBe(9);
		expect(lines[0].queuedAt).toBe(1);
	});
});

describe('scheduled turns', () => {
	const LOOP = '# Autonomous loop\nCheck the queue depth.';
	const scheduled = (body: string, ts: number, turnId: string, at?: string): AgentEvent =>
		({
			...(text(`▷ User: ${body}`, ts) as AgentEvent & { type: 'text' }),
			turn_id: turnId,
			...(at ? { metadata: { scheduled_at: at } } : {})
		}) as AgentEvent;

	it('is never classified as poll, even when its text looks like one', () => {
		const at = '2026-09-25T09:00:00Z';
		const events = [scheduled(LOOP, 1, 'a', at), scheduled(LOOP, 2, 'b', at)];
		expect(roles(events)).toEqual(['user', 'user']);
	});

	it('carries the scheduled time from the event metadata', () => {
		const [line] = buildLines([scheduled('ping', 1, 'a', '2026-09-25T09:00:00Z')], ctx());
		expect(line.role).toBe('user');
		expect(line.scheduledAt).toBe(Date.parse('2026-09-25T09:00:00Z'));
	});

	it('recognises a live scheduled turn by its turn_id', () => {
		const c = { ...ctx(), scheduledTurns: new Map([['a', '2026-09-25T09:00:00Z']]) };
		const [line] = buildLines([scheduled(LOOP, 1, 'a')], c);
		expect(line.role).toBe('user');
		expect(line.scheduledAt).toBe(Date.parse('2026-09-25T09:00:00Z'));
	});

	it('leaves an unscheduled look-alike as poll', () => {
		expect(roles([scheduled(LOOP, 1, 'a')])).toEqual(['poll']);
	});
});

describe('keep-alive ticks', () => {
	const tick = '▷ User: [cctui keep-alive 2026-09-24T12:00:00Z] Cache keep-alive tick. Reply with a single word.';

	it('collapses the tick and its reply into one hidden-by-default marker', () => {
		const events = [text('▷ User: go', 1), text('hello', 2), text(tick, 3), text('warm', 4)];
		const lines = buildLines(events, ctx());
		expect(lines.map((l) => l.role)).toEqual(['user', 'assistant', 'marker']);
		const marker = lines[2];
		expect(marker.keepalive).toBe(true);
		expect(marker.markerTexts?.at(-1)).toBe('warm');
		expect(buildLines(events, ctx({ marker: false })).map((l) => l.role)).toEqual([
			'user',
			'assistant'
		]);
		const dflt = defaultFilter();
		expect(buildLines(events, { ...ctx(), visible: (c) => dflt[c] }).map((l) => l.role)).toEqual([
			'user',
			'assistant'
		]);
	});

	it('leaves the next human turn and its reply untouched', () => {
		const lines = buildLines(
			[text(tick, 1), text('warm', 2), text('▷ User: continue', 3), text('working', 4)],
			ctx()
		);
		expect(lines.map((l) => l.role)).toEqual(['marker', 'user', 'assistant']);
		expect(lines[2].text).toBe('working');
	});
});
