import { describe, expect, it } from 'vitest';
import type { SessionListItem } from '@bindings/SessionListItem';
import {
	BRIEF_FILE_NAME,
	INLINE_BRIEF_MAX_BYTES,
	clampFollowupWhenCold,
	followupPrefill,
	followupPrompt,
	routeEnter,
	showColdOffer
} from './followup';

const src = { id: 's1', name: 'my "run"', working_dir: '/w' };

describe('followupPrompt', () => {
	it('wraps the brief, then the separator and the instruction', () => {
		const { prompt, file } = followupPrompt(src, '**User:**\n\nq', 'Now add tests.');
		expect(file).toBeNull();
		expect(prompt).toBe(
			'<previous-conversation session="my &quot;run&quot;" id="s1">\n**User:**\n\nq\n</previous-conversation>\n\n===\n\nNow add tests.'
		);
	});

	it('defaults the instruction and falls back to the cwd for an unnamed session', () => {
		const { prompt } = followupPrompt({ ...src, name: null }, 'b', '  ');
		expect(prompt.startsWith('<previous-conversation session="/w" id="s1">')).toBe(true);
		expect(prompt.endsWith('===\n\nContinue from this.')).toBe(true);
	});

	it('moves an oversized brief into the bootstrap file', () => {
		const brief = 'x'.repeat(INLINE_BRIEF_MAX_BYTES + 1);
		const { prompt, file } = followupPrompt(src, brief);
		expect(prompt).toContain(BRIEF_FILE_NAME);
		expect(prompt).not.toContain(brief);
		expect(file).toContain(brief);
	});
});

describe('followupPrefill', () => {
	const session = {
		id: 's1',
		name: 'n',
		working_dir: '/w',
		machine_id: 'm1',
		adapter_id: 'codex',
		model: 'gpt-5.6',
		effort: 'high',
		account_name: 'acct',
		permission_mode: null,
		labels: [{ id: 'l1' }, { id: 'l2' }]
	} as unknown as SessionListItem;

	it('carries the source config and the follow-up linkage', () => {
		const p = followupPrefill(session, 'brief', { archiveSource: true });
		expect(p).toMatchObject({
			machine_id: 'm1',
			working_dir: '/w',
			adapter_id: 'codex',
			model_codex: 'gpt-5.6',
			effort_codex: 'high',
			account: 'acct',
			label_ids: 'l1,l2',
			relation: 'followup',
			parent_session_id: 's1',
			archive_source: '1'
		});
		expect(p.prompt).toContain('brief');
		expect(p).not.toHaveProperty('permission_mode');
		expect(p).not.toHaveProperty('followup_file');
	});
});

describe('settings routing', () => {
	it('clamps unknown values to offer', () => {
		expect(clampFollowupWhenCold(undefined)).toBe('offer');
		expect(clampFollowupWhenCold('nope')).toBe('offer');
		expect(clampFollowupWhenCold('default')).toBe('default');
		expect(clampFollowupWhenCold('off')).toBe('off');
	});

	it('routes Enter to the follow-up only when cold under default', () => {
		expect(routeEnter('default', true, false)).toBe('followup');
		expect(routeEnter('default', true, true)).toBe('send');
		expect(routeEnter('default', false, false)).toBe('send');
		expect(routeEnter('offer', true, false)).toBe('send');
		expect(routeEnter('off', true, false)).toBe('send');
	});

	it('offers only when cold, not dismissed and not off', () => {
		expect(showColdOffer('offer', true, false)).toBe(true);
		expect(showColdOffer('default', true, false)).toBe(true);
		expect(showColdOffer('off', true, false)).toBe(false);
		expect(showColdOffer('offer', true, true)).toBe(false);
		expect(showColdOffer('offer', false, false)).toBe(false);
	});
});
