import type { SessionListItem } from '@bindings/SessionListItem';

export type FollowupWhenCold = 'off' | 'offer' | 'default';

export const FOLLOWUP_RELATION = 'followup';
export const BRIEF_FILE_NAME = 'previous-conversation.md';
export const INLINE_BRIEF_MAX_BYTES = 48 * 1024;
export const BRIEF_FETCH_MAX_BYTES = 512 * 1024;
export const DEFAULT_INSTRUCTION = 'Continue from this.';

export function clampFollowupWhenCold(v: unknown): FollowupWhenCold {
	return v === 'off' || v === 'default' ? v : 'offer';
}

function attr(v: string): string {
	return v.replace(/&/g, '&amp;').replace(/"/g, '&quot;').replace(/</g, '&lt;');
}

function byteLength(s: string): number {
	return new TextEncoder().encode(s).length;
}

export interface FollowupPrompt {
	prompt: string;
	file: string | null;
}

export function followupPrompt(
	session: Pick<SessionListItem, 'id' | 'name' | 'working_dir'>,
	brief: string,
	instruction: string = DEFAULT_INSTRUCTION
): FollowupPrompt {
	const name = attr(session.name || session.working_dir);
	const open = `<previous-conversation session="${name}" id="${attr(session.id)}">`;
	const ask = instruction.trim() || DEFAULT_INSTRUCTION;
	if (byteLength(brief) > INLINE_BRIEF_MAX_BYTES) {
		return {
			prompt: `${open}\nThe transcript is in the attached file ${BRIEF_FILE_NAME}. Read it first.\n</previous-conversation>\n\n===\n\n${ask}`,
			file: `${open}\n${brief}\n</previous-conversation>\n`
		};
	}
	return { prompt: `${open}\n${brief}\n</previous-conversation>\n\n===\n\n${ask}`, file: null };
}

export function followupPrefill(
	session: SessionListItem,
	brief: string,
	opts: { instruction?: string; archiveSource?: boolean } = {}
): Record<string, string> {
	const adapter = session.adapter_id ?? 'claude-code';
	const codex = adapter === 'codex';
	const { prompt, file } = followupPrompt(session, brief, opts.instruction);
	const full: Record<string, string> = {
		machine_id: session.machine_id,
		working_dir: session.working_dir,
		adapter_id: adapter,
		name: '',
		prompt,
		[codex ? 'model_codex' : 'model_claude']: session.model ?? '',
		[codex ? 'effort_codex' : 'effort_claude']: session.effort ?? '',
		account: session.account_name ?? '',
		permission_mode: session.permission_mode ?? '',
		label_ids: session.labels.map((l) => l.id).join(','),
		relation: FOLLOWUP_RELATION,
		parent_session_id: session.id,
		archive_source: opts.archiveSource ? '1' : '',
		followup_file: file ?? ''
	};
	return Object.fromEntries(Object.entries(full).filter(([, v]) => v !== ''));
}

export type EnterRoute = 'send' | 'followup';

// Shift+Enter always sends in place.
export function routeEnter(setting: FollowupWhenCold, cold: boolean, shift: boolean): EnterRoute {
	if (shift || !cold || setting !== 'default') return 'send';
	return 'followup';
}

export function showColdOffer(setting: FollowupWhenCold, cold: boolean, dismissed: boolean): boolean {
	return cold && !dismissed && setting !== 'off';
}
