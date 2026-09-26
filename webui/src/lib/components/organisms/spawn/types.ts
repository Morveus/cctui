// Shared spawn-form types, extracted from SpawnModal (no behavior change).
import type { PermissionMode } from '@bindings/PermissionMode';

// "machine" = spawn on an enrolled daemon; "dispatch" = hand off to a k8s
// dispatcher (claude-worker) that runs the session in an ephemeral pod.
export type Target = 'machine' | 'dispatch';

export interface Form {
	machine_id: string;
	adapter_id: string;
	working_dir: string;
	name: string;
	prompt: string;
	// Empty = "Default" (unset): the server then applies the account default
	// permission mode, falling back to claude's own default when the account has
	// none. A concrete value is a per-spawn override.
	permission_mode: PermissionMode | '';
	// dispatch-only fields (forwarded to the dispatcher as `payload`).
	dispatcher: string;
	// The harness the dispatched worker runs: 'claude-code' (default,
	// backward compatible) drives a claude worker via the control socket; 'codex'
	// runs headless `codex exec` in the worker. Selects which model/effort field
	// applies, mirroring the machine-tab adapter.
	dispatch_adapter: string;
	identity: string;
	repo: string;
	ticket: string;
	prompt_file: string;
	// Model family is per-adapter (claude families vs codex models), like
	// effort below, so each gets its own field and they survive an adapter
	// switch. Dispatch (k8s) runs a claude worker → model_claude.
	model_claude: string;
	model_codex: string;
	// Named account to run under. Empty = "Default (no
	// account)" → the worker's own auth, adapter-first flow. When set, the
	// account drives the model list + locks the harness, and `account_provider`
	// disambiguates a name shared across providers at spawn.
	account: string;
	account_provider: string;
	// Free-form model for a compatible-endpoint account: the picker is
	// driven by the account's declared models rather than the per-adapter family
	// lists, so it gets its own field (preserved across account switches).
	model_account: string;
	// Effort is per-adapter (claude and codex have different level sets), so each
	// gets its own slider + form field and they're preserved across an adapter
	// switch. Dispatch (k8s) runs a claude worker → uses effort_claude.
	effort_claude: string;
	effort_codex: string;
	// Codex service tier: '' leaves it to the account default, 'fast' opts this
	// one session into the priority tier. Codex-only.
	service_tier: string;
	timeout: string;
	// Context pack: a git repo the worker clones at boot, delivered as
	// CONTEXT_PACK_* env vars on the opaque dispatch payload. The URL accepts the
	// `@<ref>`/`#<subdir>` shorthand, so the three advanced fields are only for
	// pinning them explicitly. The token may be a `vault:`/`k8s:` ref and falls
	// back to GITHUB_TOKEN.
	context_pack_url: string;
	context_pack_ref: string;
	context_pack_subdir: string;
	context_pack_token: string;
	// Label ids to attach to the spawned session. Remembered between
	// New Session opens via LAST_SPAWN_LABELS so the next spawn defaults to the
	// last-used set. Resolved against the live label list for display.
	labels: string[];
}

/** What opens the form pre-seeded: a session's config, or a draft to edit
 * (`draft_id` names its row, `env_keys` the comma-joined env var names,
 * `label_ids` the comma-joined label ids). */
export type SpawnPrefill = Partial<Form> & {
	draft_id?: string;
	env_keys?: string;
	label_ids?: string;
	relation?: string;
	parent_session_id?: string;
	archive_source?: string;
	followup_file?: string;
};

/** One env-secret row of the spawn form. Values never reach a draft. */
export interface EnvRow {
	key: string;
	value: string;
}
