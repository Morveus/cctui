<script lang="ts">
	import { onDestroy } from 'svelte';
	import { imageAttachments } from '$lib/imageAttachments.svelte';
	const images = imageAttachments();
	onDestroy(() => images.reset());
	import { ApiError, errMessage } from '$lib/api';
	import type { SpawnRequest } from '@bindings/SpawnRequest';
	import type { SessionProfile } from '@bindings/SessionProfile';
	import {
		useAllMachines,
		useDispatchers,
		useSessionActions,
		useRecentDirs,
		useAccounts,
		useAccountPools,
		useAllAccountsUsage,
		useLabels,
		useProfiles,
		useProfileActions,
		endpoints
	} from '$lib/queries';
	import { ws, type SpawnProbeHit } from '$lib/ws.svelte';
	import { toasts } from '$lib/toast.svelte';
	import { goto } from '$app/navigation';
	import { isQueuedSpawn, notifyQueuedSpawn } from '$lib/memCeiling';
	import { isSubmitChord, submitChordLabel } from '$lib/platform';
	import {
		drafts,
		promptHistory,
		SPAWN_SLOT,
		spawnSlotKey,
		currentSpawnSlot,
		readSpawnSlot,
		type SpawnSlotPayload,
		LAST_MACHINE,
		LAST_SPAWN_NAME,
		LAST_SPAWN_LABELS,
		nextSessionName,
		normalizeDir
	} from '$lib/drafts';
	import {
		machineMemoryKey,
		dispatchMemoryKey,
		applyMemory,
		dirPrefill,
		labelPrefill,
		memoryFieldsOf,
		entryFromForm,
		recordProfileUse,
		PROFILE_USES,
		MACHINE_MEMORY_FIELDS,
		DISPATCH_MEMORY_FIELDS,
		type MemoryPatch
	} from '$lib/spawnMemory';
	import { attachFiles, removeFileByName, fileCapError } from '$lib/attachments';
	import { attachmentStore, dropMissingTokens } from '$lib/attachmentStore';
	import { AutoGrid, Button, Callout, Dropzone, Modal, OptionButton, resizeHandle, Text } from '@dorsk/tsumikit';
	import { dialogBackdropGuard } from '$lib/dialogBackdropGuard';
	import MachineFields from './spawn/MachineFields.svelte';
	import DispatchFields from './spawn/DispatchFields.svelte';
	import ProfileList from './spawn/ProfileList.svelte';
	import SpawnAddons from './spawn/SpawnAddons.svelte';
	import type { EnvRow, Form, SpawnPrefill, Target } from './spawn/types';
	import {
		accountBacksAdapter,
		providerForAdapter,
		isCompatibleProvider,
		NO_ACCOUNT,
		poolName
	} from './spawn/options';
	import {
		applySpec,
		initialProfile,
		specFromForm,
		specOf,
		uniqueProfileName,
		type ProfileSpec
	} from './spawn/profiles';
	import { buildDispatchBody } from './spawn/dispatchBody';
	import { attachLabelsTo, attachLabelsToSpawned } from './spawn/labelAttach';
	import { settings, type SpawnDockSide } from '$lib/settings.svelte';
	import { SPAWN_DOCK_WIDTH } from '$lib/spawnDock.svelte';
	import { DOCK_MIN_PX, maxDockWidth } from '$lib/dock';
	import { m } from '$lib/paraglide/messages';

	let dragging = $state(false);
	let viewportWidth = $state(0);
	const maxPx = $derived(maxDockWidth(viewportWidth));

	let {
		onclose,
		onspawned,
		prefill = null,
		docked = null,
		stacked = false,
		dockWidth = SPAWN_DOCK_WIDTH,
		autosaveDelay = 2000
	}: {
		// Modal: close the dialog. Docked: the form is done with (spawned, saved
		// as draft, or cleared) — the parent remounts it so it reseeds exactly the
		// way a reopened modal would.
		onclose: () => void;
		onspawned: () => void;
		// Docked panel (Settings › New session): the same form pinned to one edge
		// of the Sessions screen instead of inside a Modal.
		docked?: SpawnDockSide | null;
		// Docked and sharing its column with the stats panel: top half only.
		stacked?: boolean;
		// Docked: the width the layout reserved on that edge (resolveDocks).
		dockWidth?: string;
		// "New session from same script" / "Edit draft": seed the form from a
		// session's config or a draft row. Non-empty prefill values win over the
		// target's local slot; empty ones never clear what the slot holds.
		prefill?: SpawnPrefill | null;
		// Quiet time before the form is mirrored to its server draft.
		autosaveDelay?: number;
	} = $props();

	const machines = useAllMachines(() => true);
	const dispatchers = useDispatchers(() => true);
	const dispatcherIds = $derived(dispatchers.data ?? []);
	const canDispatch = $derived(dispatcherIds.length > 0);

	let target = $state<Target>('machine');
	const targetOptions = $derived([
		{ value: 'machine', label: m.spawn_tab_machine() },
		{ value: 'dispatch', label: m.spawn_tab_dispatch() }
	]);

	const blank: Form = {
		machine_id: '',
		adapter_id: 'claude-code',
		working_dir: '',
		name: '',
		prompt: '',
		permission_mode: '' as Form['permission_mode'],
		dispatcher: '',
		dispatch_adapter: 'claude-code',
		identity: '',
		repo: '',
		ticket: '',
		prompt_file: '',
		model_claude: '',
		model_codex: '',
		model_account: '',
		account: '',
		account_provider: '',
		effort_claude: '',
		effort_codex: '',
		service_tier: '',
		timeout: '',
		context_pack_url: '',
		context_pack_ref: '',
		context_pack_subdir: '',
		context_pack_token: '',
		labels: []
	};
	// Local autosave slot, one per (machine, cwd): a prefill names its target's
	// slot, a plain open resumes the slot last in progress. The slot's server
	// mirror is the draft row `draftId`, created on the first autosave.
	// svelte-ignore state_referenced_locally
	let slotKey =
		prefill?.machine_id && prefill.working_dir
			? spawnSlotKey(prefill.machine_id, prefill.working_dir)
			: currentSpawnSlot();
	const loadKey = slotKey;
	let draftId = $state<string | null>(null);
	let loadedDraft = false;
	let restoredEnvRows: EnvRow[] = [];
	let form = $state<Form>(load());
	// What the modal seeded: the baseline the memory effects compare against so
	// an explicit user edit is never clobbered. A one-time snapshot.
	// svelte-ignore state_referenced_locally
	const initialFields = memoryFieldsOf(form);
	// svelte-ignore state_referenced_locally
	const initialDir = form.working_dir;
	// svelte-ignore state_referenced_locally
	const initialLabels = [...form.labels];
	let labelsApplied: string[] | null = null;
	function load(): Form {
		try {
			const saved: SpawnSlotPayload = readSpawnSlot(slotKey) ?? {};
			const raw = Object.keys(saved).length > 0;
			const { draft_id, env_keys, ...prefillForm } = prefill ?? {};
			draftId = draft_id ?? saved.draftId ?? null;
			loadedDraft = (raw || !!draft_id) && !(prefill && !draft_id);
			// Values never come back from disk: only the keys are re-proposed.
			const keys = new Set<string>();
			for (const r of saved.envRows ?? []) if (r?.key) keys.add(String(r.key));
			for (const k of env_keys?.split(',') ?? []) if (k) keys.add(k);
			restoredEnvRows = [...keys].map((key) => ({ key, value: '' }));
			const {
				envRows: _envRows,
				draftId: _draftId,
				attachmentNames: _names,
				...savedForm
			} = saved;
			const given = Object.fromEntries(
				Object.entries(prefillForm).filter(([, v]) => v !== '' && v != null)
			);
			const seeded = { ...blank, ...savedForm, ...given } as Form;
			// Fresh open: propose the last submitted name with a bumped suffix and
			// the last-used label set.
			if (!raw && !prefill) {
				const lastName = drafts.get(LAST_SPAWN_NAME);
				if (lastName) seeded.name = nextSessionName(lastName);
				if (!savedForm.labels) {
					seeded.labels = (drafts.get(LAST_SPAWN_LABELS) ?? '').split(',').filter(Boolean);
				}
			}
			return seeded;
		} catch {
			const { draft_id: _d, env_keys: _k, ...rest } = prefill ?? {};
			return { ...blank, ...rest } as Form;
		}
	}

	$effect(() => {
		const list = machines.data ?? [];
		if (form.machine_id || !list.length) return;
		const last = drafts.get(LAST_MACHINE);
		form.machine_id = last && list.some((m) => m.id === last) ? last : list[0].id;
	});

	// Spawn memory: the config last submitted for a (machine, cwd), recalled
	// whenever the machine or cwd changes. An explicit edit in the open modal
	// (or a restored draft / prefill) wins over the memory, which wins over the
	// blank seed. `memApplied` tracks what the memory wrote so applyMemory can
	// tell an edit from its own writes.
	let memApplied: MemoryPatch = {};

	// Picking a machine pre-fills its most recent working dir. Keyed on
	// (machine, remembered dir) so the fill re-attempts once the settings blob
	// hydrates. A draft/prefill only suppresses the fill when it carries a dir.
	let dirComboApplied: string | null = null;
	let dirApplied: string | null = null;
	$effect(() => {
		const id = form.machine_id;
		if (!id) return;
		const last = settings.lastDirFor(id) ?? recentDirs[0] ?? null;
		const combo = machineMemoryKey(id, last ?? '');
		if (combo === dirComboApplied) return;
		const first = dirComboApplied === null;
		dirComboApplied = combo;
		if (first && (prefill?.working_dir || (loadedDraft && initialDir))) return;
		const next = dirPrefill(form.working_dir, last, dirApplied ?? initialDir);
		if (next === null) return;
		dirApplied = next;
		form.working_dir = next;
	});

	let memKeyApplied = $state<string | null>(null);
	$effect(() => {
		const key = form.machine_id ? machineMemoryKey(form.machine_id, form.working_dir) : null;
		if (!key || key === memKeyApplied) return;
		const first = memKeyApplied === null;
		memKeyApplied = key;
		if (first && (prefill || loadedDraft)) return;
		const entry = settings.recallSpawn(key) ?? settings.lastEntryFor(form.machine_id);
		if (!entry) return;
		const patch = applyMemory(
			MACHINE_MEMORY_FIELDS,
			form,
			initialFields,
			memApplied,
			entry,
			drafts.get(LAST_SPAWN_NAME)
		);
		memApplied = { ...memApplied, ...patch };
		Object.assign(form, patch as Partial<Form>);
		const labels = labelPrefill(form.labels, initialLabels, labelsApplied, entry);
		if (labels) {
			labelsApplied = labels;
			form.labels = labels;
		}
	});

	let dispatchKeyApplied = $state<string | null>(null);
	$effect(() => {
		const key = form.dispatcher ? dispatchMemoryKey(form.dispatcher, form.repo) : null;
		if (!key || key === dispatchKeyApplied) return;
		const first = dispatchKeyApplied === null;
		dispatchKeyApplied = key;
		if (first && (prefill || loadedDraft)) return;
		const entry = settings.recallSpawn(key);
		if (!entry) return;
		const patch = applyMemory(
			DISPATCH_MEMORY_FIELDS,
			form,
			initialFields,
			memApplied,
			entry,
			drafts.get(LAST_SPAWN_NAME)
		);
		memApplied = { ...memApplied, ...patch };
		Object.assign(form, patch as Partial<Form>);
	});

	$effect(() => {
		if (form.dispatcher || !dispatcherIds.length) return;
		form.dispatcher = dispatcherIds[0];
	});

	const dirsQuery = useRecentDirs(() => form.machine_id);
	const recentDirs = $derived([...new Set((dirsQuery.data ?? []).map(normalizeDir))]);

	const accounts = useAccounts(() => true);
	const pools = useAccountPools(() => true);
	const usageQuery = useAllAccountsUsage(() => true);
	const allAccounts = $derived(accounts.data ?? []);
	const allPools = $derived(pools.data ?? []);
	const allUsage = $derived(usageQuery.data ?? []);

	const labelsQuery = useLabels();
	const allLabels = $derived(labelsQuery.data?.labels ?? []);
	const actions = useSessionActions();
	let busy = $state(false);

	// --- Profiles ---
	// The selected profile's kit (or its one-off adjustment) is written over
	// the form at submit time: profile → one-off → explicit prompt / where.
	const profilesQuery = useProfiles();
	const profileActions = useProfileActions();
	const profiles = $derived(profilesQuery.data ?? []);
	let selectedProfileId = $state<string | null>(null);
	let oneOff = $state<ProfileSpec | null>(null);
	let usageRaw = $state(drafts.get(PROFILE_USES));
	const selectedProfile = $derived(profiles.find((p) => p.id === selectedProfileId) ?? null);
	const profileSpec = $derived(oneOff ?? (selectedProfile ? specOf(selectedProfile) : null));
	const effectiveForm = $derived(
		target === 'machine' && profileSpec ? applySpec(form, profileSpec, allAccounts, allPools) : form
	);

	// Each machine opens on the profile it last spawned from.
	let profileMachineApplied: string | null = null;
	$effect(() => {
		if (!profiles.length) return;
		const machine = form.machine_id;
		const stillThere = !!selectedProfileId && profiles.some((p) => p.id === selectedProfileId);
		if (machine === profileMachineApplied && stillThere) return;
		profileMachineApplied = machine;
		const last = settings.lastEntryFor(machine)?.profile_id ?? null;
		selectedProfileId = initialProfile(profiles, last)?.id ?? null;
		oneOff = null;
	});

	// First open with no profile yet: seed "Default" from the spawn memory.
	let seeded = false;
	$effect(() => {
		if (seeded || profilesQuery.data === undefined || profiles.length) return;
		if (!form.machine_id || accounts.data === undefined || pools.data === undefined) return;
		seeded = true;
		void profileActions
			.create({ name: m.spawn_profile_default_name(), ...specFromForm(form, allAccounts, allPools) })
			.catch(() => {});
	});

	async function createProfile() {
		const base = profileSpec ?? specFromForm(form, allAccounts, allPools);
		const name = uniqueProfileName(
			m.spawn_profile_new_name(),
			profiles.map((p) => p.name)
		);
		try {
			const p = await profileActions.create({ name, ...base });
			selectedProfileId = p.id;
			oneOff = null;
		} catch (e) {
			toasts.error(m.spawn_profile_toast_failed({ error: errMessage(e) }));
		}
	}
	async function saveProfile(id: string, name: string, spec: ProfileSpec) {
		try {
			await profileActions.update(id, { name, spec });
			toasts.ok(m.spawn_profile_toast_saved());
		} catch (e) {
			toasts.error(m.spawn_profile_toast_failed({ error: errMessage(e) }));
		}
	}
	async function deleteProfile(id: string) {
		try {
			await profileActions.remove(id);
			if (selectedProfileId === id) {
				selectedProfileId = null;
				oneOff = null;
			}
		} catch (e) {
			toasts.error(m.spawn_profile_toast_failed({ error: errMessage(e) }));
		}
	}
	function rememberProfileUse(p: SessionProfile | null) {
		if (!p) return;
		usageRaw = recordProfileUse(usageRaw, p.id);
		drafts.set(PROFILE_USES, usageRaw);
	}

	const selectedAccount = $derived(
		effectiveForm.account &&
			effectiveForm.account !== NO_ACCOUNT &&
			!poolName(effectiveForm.account)
			? allAccounts.find((a) => a.name === effectiveForm.account)
			: undefined
	);
	const spawnProvider = $derived(
		providerForAdapter(selectedAccount, effectiveForm.adapter_id)?.provider
	);
	const harnessValid = $derived(accountBacksAdapter(selectedAccount, effectiveForm.adapter_id));
	const dispatchProvider = $derived(
		providerForAdapter(selectedAccount, form.dispatch_adapter || 'claude-code')?.provider
	);

	// --- Environment secrets & file uploads ---
	// Kept out of `form` (persisted to localStorage drafts) so secret values
	// never reach disk; only env keys go into the draft. Files live in
	// IndexedDB (attachmentStore), keyed like the draft.
	let envRows = $state<EnvRow[]>(restoredEnvRows);
	let files = $state<File[]>([]);
	let filesRestored = $state(false);
	$effect(() => {
		const key = form.machine_id ? spawnSlotKey(form.machine_id, form.working_dir) : slotKey;
		if (key !== slotKey) {
			drafts.clear(slotKey);
			if (filesRestored) void attachmentStore.clear(slotKey);
			slotKey = key;
		}
		const envKeys = envRows.map((r) => ({ key: r.key, value: '' }));
		const { context_pack_token: _packToken, ...persisted } = form;
		const payload: SpawnSlotPayload = {
			...persisted,
			envRows: envKeys,
			draftId,
			attachmentNames: files.map((f) => f.name)
		};
		drafts.set(slotKey, JSON.stringify(payload));
		drafts.set(SPAWN_SLOT, slotKey);
		if (filesRestored) void attachmentStore.set(slotKey, [...files]);
	});
	$effect(() => {
		let live = true;
		(async () => {
			const restored = await attachmentStore.get(loadKey);
			if (!live) return;
			files = restored.files;
			const { text, dropped } = dropMissingTokens(form.prompt, restored.missing);
			if (dropped) {
				form.prompt = text;
				toasts.info(m.attachments_missing_dropped({ count: dropped }));
			}
			filesRestored = true;
			if (loadKey !== slotKey) void attachmentStore.clear(loadKey);
		})();
		return () => {
			live = false;
		};
	});

	// Server autosave: `autosaveDelay` after the last edit the form is written
	// to its draft row (created on first save, updated in place after).
	let autosaveTimer: ReturnType<typeof setTimeout> | null = null;
	let autosaving = false;
	let autosaveSnapshot: string | null = null;
	$effect(() => {
		const snapshot = JSON.stringify({
			form,
			keys: envRows.map((r) => r.key),
			names: files.map((f) => f.name)
		});
		if (snapshot === autosaveSnapshot) return;
		const first = autosaveSnapshot === null;
		autosaveSnapshot = snapshot;
		if (first) return;
		if (autosaveTimer) clearTimeout(autosaveTimer);
		autosaveTimer = setTimeout(() => void autosave(), autosaveDelay);
	});
	$effect(() => () => cancelAutosave());
	function cancelAutosave() {
		if (autosaveTimer) clearTimeout(autosaveTimer);
		autosaveTimer = null;
	}
	function draftBody(): SpawnRequest {
		return {
			...buildSpawnBody(),
			env: {},
			env_keys: envRows.map((r) => r.key.trim()).filter(Boolean),
			attachment_names: files.map((f) => f.name)
		};
	}
	async function autosave(): Promise<boolean> {
		cancelAutosave();
		if (busy || autosaving || !autosaveReady) return false;
		autosaving = true;
		try {
			const body = draftBody();
			if (draftId) {
				try {
					await actions.updateDraft(draftId, body);
					return true;
				} catch (e) {
					if (!(e instanceof ApiError && e.status === 404)) throw e;
					draftId = null;
				}
			}
			const res = await actions.spawn({ ...body, save_draft: true }, []);
			draftId = String(res.command_id);
			return true;
		} catch (e) {
			toasts.error(m.spawn_toast_save_draft_failed({ error: errMessage(e) }));
			return false;
		} finally {
			autosaving = false;
		}
	}

	/** Whether the form holds anything the user would miss. */
	export function isDirty(): boolean {
		return (
			!!form.prompt.trim() ||
			!!form.name.trim() ||
			envRows.some((r) => r.key.trim()) ||
			files.length > 0
		);
	}
	/** The server draft this form mirrors, once autosaved. */
	export function currentDraftId(): string | null {
		return draftId;
	}
	/** Write the form to its draft row now; false when it can't be a draft yet. */
	export function flushDraft(): Promise<boolean> {
		return autosave();
	}

	function resetForm() {
		cancelAutosave();
		draftId = null;
		drafts.clear(slotKey);
		drafts.clear(SPAWN_SLOT);
		form = { ...blank, machine_id: form.machine_id, dispatcher: form.dispatcher };
		envRows = [];
		files = [];
		images.reset();
		oneOff = null;
	}
	function discardMirror() {
		if (!draftId) return;
		const id = draftId;
		draftId = null;
		actions.discardDraft(id).catch(() => {});
	}

	const ENV_KEY_RE = /^[A-Z_][A-Z0-9_]*$/;
	const badEnvKeys = $derived(
		envRows.filter((r) => r.key.trim() && !ENV_KEY_RE.test(r.key.trim()))
	);
	const fileError = $derived(fileCapError(files));
	const secretsValid = $derived(badEnvKeys.length === 0 && !fileError && images.pending.length === 0);

	const addFiles = (incoming: File[]) => {
		if (busy) return;
		images.add(incoming, (file) => {
			({ files, text: form.prompt } = attachFiles(files, form.prompt, [file]));
		}, (file) => toasts.error(m.attachments_compression_failed({ name: file.name })));
	};
	/** Complete rows only (both key and value set). */
	function envMap(): Record<string, string> {
		const out: Record<string, string> = {};
		for (const r of envRows) {
			const k = r.key.trim();
			if (k && r.value) out[k] = r.value;
		}
		return out;
	}
	const removeFile = (name: string) => (files = removeFileByName(files, name));

	const spawnValid = $derived(!!form.machine_id && !!form.working_dir.trim() && harnessValid);
	const dispatchValid = $derived(
		!!form.dispatcher && (!!form.prompt.trim() || !!form.prompt_file.trim())
	);
	const valid = $derived((target === 'machine' ? spawnValid : dispatchValid) && secretsValid);
	const autosaveReady = $derived(target === 'machine' && spawnValid && !!form.prompt.trim());

	function buildSpawnBody(): SpawnRequest {
		const f = effectiveForm;
		const adapter = f.adapter_id;
		const noAccount = f.account === NO_ACCOUNT;
		const pool = poolName(f.account) ?? null;
		const compatible = !!spawnProvider && isCompatibleProvider(spawnProvider);
		const model = compatible
			? f.model_account || null
			: (adapter === 'codex' ? f.model_codex : f.model_claude) || null;
		return {
			machine_id: f.machine_id,
			working_dir: normalizeDir(f.working_dir.trim()),
			adapter_id: adapter,
			name: f.name.trim() || null,
			prompt: f.prompt.trim() || null,
			prompt_name: null,
			// null lets the server resolve the account default permission mode.
			permission_mode: f.permission_mode || null,
			effort: (adapter === 'codex' ? f.effort_codex : f.effort_claude) || null,
			service_tier: (adapter === 'codex' && f.service_tier) || null,
			model,
			env: envMap(),
			account: noAccount || pool ? null : f.account.trim() || null,
			provider: noAccount || pool ? null : spawnProvider || null,
			no_account: noAccount,
			// "Auto" delegates the choice to the server; a pool is the bounded form.
			auto_account: !noAccount && !pool && !f.account.trim(),
			pool,
			save_draft: false
		};
	}

	async function saveDraft() {
		cancelAutosave();
		const body = draftBody();
		if (draftId) await actions.updateDraft(draftId, body);
		else await actions.spawn({ ...body, save_draft: true }, []);
		drafts.set(LAST_MACHINE, form.machine_id);
		drafts.set(LAST_SPAWN_NAME, form.name.trim());
		toasts.ok(m.spawn_toast_saved_draft());
		resetForm();
		onspawned();
		onclose();
	}

	let spawnFailure = $state<string | null>(null);
	async function spawnProbe(sessionId: string): Promise<SpawnProbeHit | null> {
		const { sessions } = await labelApi.listSessions();
		return sessions.find((s) => s.id === sessionId) ?? null;
	}
	const labelApi = {
		attachLabel: (sessionId: string, labelId: string) => actions.attachLabel(sessionId, labelId),
		listSessions: () => endpoints.sessions(false)
	};

	async function spawnOnMachine() {
		cancelAutosave();
		spawnFailure = null;
		const body: SpawnRequest = buildSpawnBody();
		const labelIds = [...form.labels];
		const labelCwd = normalizeDir(form.working_dir.trim());
		const labelMachine = form.machine_id;
		const profile = selectedProfile;
		const requestedAt = Date.now();
		const res = await actions.spawn(body, files);
		if (res.account) toasts.info(m.spawn_toast_bound_account({ account: res.account }));
		drafts.set(LAST_MACHINE, form.machine_id);
		drafts.set(LAST_SPAWN_NAME, form.name.trim());
		drafts.set(LAST_SPAWN_LABELS, labelIds.join(','));
		// Saved on submit (not on confirmed success) so a slow spawn still
		// records the operator's intent.
		settings.rememberSpawn(machineMemoryKey(labelMachine, labelCwd), {
			...entryFromForm(effectiveForm),
			profile_id: profile?.id
		});
		rememberProfileUse(profile);
		if (isQueuedSpawn(res)) {
			// Held back by the machine's RAM ceiling: a queued session row, no
			// command result will ever come for it. It is not a failure.
			notifyQueuedSpawn(res, (sid) => void goto(`/sessions/${sid}`));
			discardMirror();
			resetForm();
			onspawned();
			onclose();
			return;
		}
		toasts.info(m.spawn_toast_spawning());
		const sessionId = res.session_id ?? null;
		const result = await ws.awaitSpawn(res.command_id, sessionId, {
			probe: sessionId ? () => spawnProbe(sessionId) : undefined
		});
		if (result.ok) {
			toasts.ok(m.spawn_toast_spawned());
			void attachLabelsToSpawned(labelApi, labelMachine, labelCwd, requestedAt, labelIds);
			discardMirror();
			resetForm();
			onspawned();
			onclose();
		} else if (result.timedOut) {
			void attachLabelsToSpawned(labelApi, labelMachine, labelCwd, requestedAt, labelIds);
			// No confirmation ≠ failed: cold spawns routinely land after the wait.
			// Keep the draft so a real miss is one re-open away; re-submitting
			// blindly would dispatch a second agent.
			toasts.info(m.spawn_toast_unconfirmed());
			onspawned();
			onclose();
		} else {
			spawnFailure = result.error ?? m.spawn_error_unknown();
			toasts.error(m.spawn_toast_spawn_failed({ error: spawnFailure }));
		}
	}

	// Stable across retries so the server's idempotency dedup makes a
	// re-submit a genuine retry, not a second pod. Cleared on success.
	let pendingDispatchId = $state<string | null>(null);

	async function dispatchToK8s() {
		pendingDispatchId ??= crypto.randomUUID();
		const body = buildDispatchBody(form, envMap(), dispatchProvider, pendingDispatchId);
		const labelIds = [...form.labels];
		const dispatchedId = pendingDispatchId;
		const res = await actions.dispatch(body);
		drafts.set(LAST_SPAWN_NAME, form.name.trim());
		settings.rememberSpawn(dispatchMemoryKey(form.dispatcher, form.repo), entryFromForm(form));
		drafts.set(LAST_SPAWN_LABELS, labelIds.join(','));
		void attachLabelsTo(labelApi, dispatchedId, labelIds);
		toasts.ok(m.spawn_toast_dispatched({ dispatcher: res.dispatcher, handle: res.handle }));
		pendingDispatchId = null;
		discardMirror();
		resetForm();
		onspawned();
		onclose();
	}

	async function submit() {
		if (!valid || busy) return;
		busy = true;
		promptHistory.push(form.prompt);
		try {
			if (target === 'machine') await spawnOnMachine();
			else await dispatchToK8s();
		} catch (e) {
			const msg = errMessage(e);
			toasts.error(
				target === 'machine'
					? m.spawn_toast_spawn_failed({ error: msg })
					: m.spawn_toast_dispatch_failed({ error: msg })
			);
		} finally {
			busy = false;
		}
	}

	// Drafts are a machine-spawn concept: valid whenever the spawn form is;
	// secrets needn't be valid yet (entered at launch).
	const draftValid = $derived(target === 'machine' && spawnValid && images.pending.length === 0);
	async function submitDraft() {
		if (!draftValid || busy) return;
		busy = true;
		try {
			await saveDraft();
		} catch (e) {
			toasts.error(m.spawn_toast_save_draft_failed({ error: errMessage(e) }));
		} finally {
			busy = false;
		}
	}

	function clearForm() {
		discardMirror();
		resetForm();
		if (docked) onclose();
	}

	const spawnLabel = $derived(
		`${target !== 'machine' ? m.spawn_action_dispatch() : m.spawn_action_spawn()} (${submitChordLabel()})`
	);
	const noMachines = $derived(target === 'machine' && (machines.data ?? []).length === 0);
	const disabledReason = $derived(noMachines ? m.spawn_no_machines_hint() : undefined);
</script>

<!-- The form body and the action row are snippets so the Modal and the docked
     panel render the very same markup; only the chrome around them differs. -->
<svelte:window bind:innerWidth={viewportWidth} />

{#snippet targetSwitch()}
	{#if canDispatch}
		<AutoGrid min="8rem" maxCols={2} gap="var(--sp-2)" role="radiogroup" aria-label={m.spawn_run_on_label()}>
			{#each targetOptions as o (o.value)}
				<OptionButton
					row
					selected={target === o.value}
					role="radio"
					aria-checked={target === o.value}
					onclick={() => (target = o.value === 'dispatch' ? 'dispatch' : 'machine')}
				>
					<Text>{o.label}</Text>
				</OptionButton>
			{/each}
		</AutoGrid>
	{/if}
{/snippet}

{#if docked}
	<aside
		class="dock"
		class:dock-left={docked === 'left'}
		class:stacked
		aria-label={m.spawn_modal_title()}
		style:--spawn-dock-w={dockWidth}
	>
	<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
	<div
		class="grip"
		class:grip-left={docked === 'left'}
		class:dragging
		role="separator"
		tabindex="0"
		aria-orientation="vertical"
		aria-valuemin={DOCK_MIN_PX}
		aria-valuemax={maxPx}
		aria-label={m.dock_resize_grip()}
		title={m.dock_resize_grip()}
		use:resizeHandle={{
			side: docked,
			min: DOCK_MIN_PX,
			max: maxPx,
			onwidth: (px) => settings.setSpawnDock({ width: px }),
			onreset: () => settings.setSpawnDock({ width: undefined }),
			onactive: (a) => {
				dragging = a;
				document.body.classList.toggle('dock-resizing', a);
			}
		}}
	></div>
		<div class="dock-head">
			<span>{m.spawn_modal_title()}</span>
		</div>
		<div class="dock-body">{@render body()}</div>
		<div class="dock-foot">{@render footer()}</div>
	</aside>
{:else}
	<Modal title={m.spawn_modal_title()} {onclose} resizeKey="cctui_spawn_modal_width" {body} {footer} />
{/if}

{#snippet body()}
	<!-- The whole dialog is a file drop area (machine target only). -->
	<Dropzone
		overlay
		multiple
		label={m.spawn_dropzone_label()}
		disabled={target !== 'machine'}
		onfiles={addFiles}
	>
		<div
			class="stack"
			data-journey="spawn"
			use:dialogBackdropGuard
			onkeydown={(e: KeyboardEvent) => {
				if (isSubmitChord(e) && !busy && valid) {
					e.preventDefault();
					void submit();
				}
			}}
		>
			{#if canDispatch}
				<div class="switch-row">{@render targetSwitch()}</div>
			{/if}
			{#if target === 'dispatch'}
				<DispatchFields bind:form {dispatcherIds} accounts={allAccounts} onsubmit={submit} />
			{:else}
				<MachineFields
					bind:form
					machines={machines.data ?? []}
					{recentDirs}
					onsubmit={submit}
					onfiles={addFiles}
				/>
				<ProfileList
					{profiles}
					bind:selectedId={selectedProfileId}
					bind:oneOff
					accounts={allAccounts}
					pools={allPools}
					usage={allUsage}
					{usageRaw}
					machineId={form.machine_id}
					{busy}
					oncreate={createProfile}
					onsave={saveProfile}
					ondelete={deleteProfile}
				/>
			{/if}
			<SpawnAddons
				pending={images.pending}
				bind:labelIds={form.labels}
				bind:envRows
				{files}
				{allLabels}
				envInvalid={badEnvKeys.length > 0}
				attachments={target === 'machine'}
				labelActions={actions}
				onfiles={addFiles}
				onremovefile={removeFile}
			/>
		</div>
	</Dropzone>
{/snippet}
{#snippet footer()}
	{#if spawnFailure}
		<Callout tone="danger" title={m.spawn_failure_inline()} style="flex-basis:100%">
			<pre class="spawn-failure-detail">{spawnFailure}</pre>
		</Callout>
	{/if}
	<span class="foot-secondary">
		<Button onclick={clearForm}>{m.spawn_clear()}</Button>
		{#if target === 'machine'}
			<Button
				data-journey="draft"
				disabled={busy || !draftValid}
				title={disabledReason}
				onclick={submitDraft}
			>
				{m.spawn_draft()}
			</Button>
		{/if}
	</span>
	<span class="foot-primary">
		<Button
			data-journey="submit"
			variant="primary"
			grow
			loading={busy}
			disabled={busy || !valid}
			title={disabledReason}
			onclick={submit}
		>
			{spawnLabel}
		</Button>
	</span>
{/snippet}

<style>
	.stack {
		display: flex;
		flex-direction: column;
		gap: var(--sp-3);
	}
	.switch-row {
		display: block;
	}
	.spawn-failure-detail {
		margin: var(--sp-1) 0 0;
		max-height: 8rem;
		overflow: auto;
		white-space: pre-wrap;
		font-family: var(--font-mono);
		font-size: var(--text-xs);
	}
	/* Docked panel: pinned to one edge between the header and the bottom nav,
	   scrolling its body on its own. The layout reserves the same width. */
	.dock {
		position: fixed;
		top: calc(var(--header-h) + var(--safe-top));
		bottom: var(--bottom-chrome, calc(var(--nav-h) + var(--safe-bottom)));
		right: 0;
		width: var(--spawn-dock-w);
		display: flex;
		flex-direction: column;
		background: var(--bg-elevated);
		border-left: 1px solid var(--border);
		z-index: 4;
	}
	.dock.dock-left {
		right: auto;
		left: 0;
		border-left: 0;
		border-right: 1px solid var(--border);
	}
	.dock.stacked {
		bottom: 50%;
	}
	/* A 10px hit area straddling the panel's border, with a 2px line that only
	   shows on hover, focus or while dragging. */
	.grip {
		position: absolute;
		top: 0;
		bottom: 0;
		left: -5px;
		width: 10px;
		cursor: ew-resize;
		touch-action: none;
		z-index: 1;
	}
	.grip-left {
		left: auto;
		right: -5px;
	}
	/* An always-visible knob in the middle of the edge, like the kit's panel handle. */
	.grip::before {
		content: '';
		position: absolute;
		top: 50%;
		left: 3px;
		width: 4px;
		height: 2.5rem;
		margin-top: -1.25rem;
		border-radius: var(--r-pill);
		background: var(--border-strong);
	}
	.grip:hover::before,
	.grip.dragging::before {
		background: var(--accent);
	}
	.grip::after {
		content: '';
		position: absolute;
		top: 0;
		bottom: 0;
		left: 4px;
		width: 2px;
		background: var(--accent);
		opacity: 0;
		transition: opacity 0.12s var(--ease);
	}
	.grip:hover::after,
	.grip:focus-visible::after,
	.grip.dragging::after {
		opacity: 1;
	}
	.grip:focus-visible {
		outline: none;
	}
	@media (hover: none) {
		.grip::after {
			opacity: 0.35;
		}
	}
	.dock-head {
		flex: none;
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--sp-2);
		padding: var(--sp-3) var(--sp-3) var(--sp-2);
		font-weight: var(--fw-semibold);
		border-bottom: 1px solid var(--border);
	}
	.dock-body {
		flex: 1;
		min-height: 0;
		overflow-y: auto;
		padding: var(--sp-3);
	}
	.dock-foot {
		flex: none;
		display: flex;
		flex-wrap: wrap;
		gap: var(--sp-2);
		padding: var(--sp-2) var(--sp-3);
		border-top: 1px solid var(--border);
	}
	.foot-secondary {
		display: flex;
		gap: var(--sp-2);
		flex: 0 1 auto;
	}
	/* The basis is the width below which the primary action would be squeezed:
	   past it the row wraps and grow makes it span the whole footer. */
	.foot-primary {
		display: flex;
		flex: 1 1 11rem;
	}
</style>
