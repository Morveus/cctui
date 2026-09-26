import { browser } from "$app/environment";
import { api } from "./api";
import { auth } from "./auth.svelte";
import {
  clampLocale,
  locale as localeStore,
  type Locale,
} from "./locale.svelte";
import { themeMode } from "./themeMode.svelte";
import { preferenceFrom, type ThemeChoice } from "./themeMode";
import { fontScale, nearestLevel } from "./fontscale.svelte";
import { notify } from "./notify.svelte";
import type { SettingsPayload } from "@bindings/SettingsPayload";
import { clampDockWidth } from "./dock";
import { clampFollowupWhenCold, type FollowupWhenCold } from "./followup";
import {
  latestDirFor,
  latestEntryFor,
  putSpawnMemory,
  type SpawnMemoryEntry,
  type SpawnMemoryMap,
} from "./spawnMemory";

// Server-persisted, user-scoped app settings. The whole preference catalogue
// lives in a single JSON blob behind GET/PUT
// /api/v1/settings, mirrored into localStorage for instant paint + offline
// fallback. The `settings` singleton is the single source of truth for the
// webui; the legacy theme/fontScale/notify singletons remain the runtime
// drivers for those three and are simply MIRRORED into this blob (the Settings
// panel drives both).
const KEY = "cctui_settings";

// Bumped when the persisted shape changes; `migrate()` walks an older payload up
// to this version. v1 is the initial schema (no migrations yet).
export const CURRENT_VERSION = 1;

// Debounce window for the PUT — coalesces a burst of toggles into one write.
const SAVE_DEBOUNCE_MS = 400;

export interface SessionListSettings {
  sort: "activity" | "created" | "name";
  sortDir: SortDir;
  view: "list" | "card";
  density: "compact" | "normal";
  section: string;
  labelFilter: string[];
  // Card accent color and section grouping share the dimension enum of
  // sessions.logic.ts; grouping has no "off" — 'status' is the bucketed list.
  colorBy: "none" | "label" | "working_dir" | "machine";
  groupBy: "status" | "label" | "working_dir" | "machine";
  // How wide the centered session-list column is allowed to grow. Only bites on
  // screens wider than the chosen cap, so it is a desktop-only knob in practice:
  // a phone viewport is already narrower than the default.
  width: SessionListWidth;
  // Show the account NAME next to each row instead of the key glyph. Off by
  // default (the glyph keeps the row terse); worth turning on when several
  // accounts of the same provider are in play, where every glyph looks alike.
  accountNames: boolean;
}

export const SORT_DIRS = ["asc", "desc"] as const;
export type SortDir = (typeof SORT_DIRS)[number];
export const DEFAULT_SORT_DIR: SortDir = "desc";
export function clampSortDir(v: unknown): SortDir {
  return (SORT_DIRS as readonly unknown[]).includes(v)
    ? (v as SortDir)
    : DEFAULT_SORT_DIR;
}

// Session-list column widths, as the `size` handed to the layout Container.
// `default` keeps --content-wide (the width every other screen uses).
export const SESSION_LIST_WIDTHS = [
  "default",
  "wide",
  "ultra",
  "full",
] as const;
export type SessionListWidth = (typeof SESSION_LIST_WIDTHS)[number];
export const DEFAULT_SESSION_LIST_WIDTH: SessionListWidth = "default";

const GROUP_BY_VALUES = ["status", "label", "working_dir", "machine"] as const;
/** Blobs written before grouping had a status mode stored 'none' for it. */
export function clampGroupBy(v: unknown): SessionListSettings["groupBy"] {
  return (GROUP_BY_VALUES as readonly unknown[]).includes(v)
    ? (v as SessionListSettings["groupBy"])
    : "status";
}

/** The CSS length for a width choice, or `undefined` to keep --content-wide. */
export function sessionListWidthSize(w: SessionListWidth): string | undefined {
  switch (w) {
    case "wide":
      return "80rem";
    case "ultra":
      return "92rem";
    case "full":
      return "100%";
    default:
      return undefined;
  }
}

/** Clamp an arbitrary stored value to a known width (an older/corrupt blob must
 *  not leak an invalid length into the Container's inline style). */
export function clampSessionListWidth(v: unknown): SessionListWidth {
  return SESSION_LIST_WIDTHS.includes(v as SessionListWidth)
    ? (v as SessionListWidth)
    : DEFAULT_SESSION_LIST_WIDTH;
}

// Docked spawn panel: instead of the "+ New" button opening a modal, the whole
// new-session form stays pinned to one edge of the Sessions screen so a serial
// spawner never opens a dialog. Off by default. Desktop-only in practice: a
// narrow viewport falls back to the modal whatever is stored here.
export const SPAWN_DOCK_SIDES = ["left", "right"] as const;
export type SpawnDockSide = (typeof SPAWN_DOCK_SIDES)[number];
export const DEFAULT_SPAWN_DOCK_SIDE: SpawnDockSide = "right";

export interface SpawnDockSettings {
  enabled: boolean;
  side: SpawnDockSide;
  /** Width in px once the user has dragged the panel's grip; unset = default. */
  width?: number;
}

// Docked stats panel: the per-account usage gauges, the rolling token windows
// and the Overview figures, pinned to one edge of the Sessions screen. Same
// on/off + side shape as the spawn dock; when both share an edge they stack.
export interface StatsDockSettings {
  enabled: boolean;
  side: SpawnDockSide;
  /** Width in px once the user has dragged the panel's grip; unset = default. */
  width?: number;
}

// Header resource gauge: the machines whose CPU / memory / disk the header
// shows a battery-style gauge for. Serializes as `data.resourceMonitor`; the
// server stores the blob untouched. Empty (the default) hides the strip.
export interface ResourceMonitorSettings {
  /** Machine ids ticked in Settings › Resource monitoring. */
  machines: string[];
}

/** Keep only well-formed, de-duplicated machine ids from a stored blob. */
export function clampMonitoredMachines(v: unknown): string[] {
  if (!Array.isArray(v)) return [];
  const out: string[] = [];
  for (const id of v) {
    if (typeof id === "string" && id.length > 0 && !out.includes(id))
      out.push(id);
  }
  return out;
}

/** Clamp a stored side to a known one (an older/corrupt blob must not pin the
 *  panel nowhere). */
export function clampSpawnDockSide(v: unknown): SpawnDockSide {
  return SPAWN_DOCK_SIDES.includes(v as SpawnDockSide)
    ? (v as SpawnDockSide)
    : DEFAULT_SPAWN_DOCK_SIDE;
}

// Horizontal placement of the toast stack (the "Archived" confirmation and
// friends). Centered by default, which is where the stack has always been;
// `left`/`right` pin it to that edge with the same gutter the centered stack
// keeps on a narrow viewport. Purely presentational, so it lives entirely in
// the webui: the value drives the `data-toast-pos` attribute on the Toaster host.
export const TOAST_POSITIONS = ["center", "left", "right"] as const;
export type ToastPosition = (typeof TOAST_POSITIONS)[number];
export const DEFAULT_TOAST_POSITION: ToastPosition = "center";

/** Clamp a stored position to a known one, so an older/corrupt blob leaves the
 *  toast centered rather than unplaced. */
export function clampToastPosition(v: unknown): ToastPosition {
  return TOAST_POSITIONS.includes(v as ToastPosition)
    ? (v as ToastPosition)
    : DEFAULT_TOAST_POSITION;
}

export interface DisplaySettings {
  /** The theme currently painted (resolved from the preference below). Kept
   *  so older builds and the first paint still find a plain id here. */
  theme: string;
  /** `auto` follows the system's colour scheme; `light` / `dark` pin a slot.
   *  Absent in blobs written before the auto mode: `theme` then pins its own slot. */
  themeMode?: ThemeChoice;
  /** Last light theme the user picked (what `auto` paints by day). */
  lightTheme?: string;
  /** Last dark theme the user picked (what `auto` paints by night). */
  darkTheme?: string;
  fontScale: number;
  // Cmd/Ctrl+E in an open conversation interrupts any in-flight turn and then
  // archives the session (Beeper/Slack-style archive chord). Preserved from the
  // previous localStorage-only Settings.
  archiveShortcut: boolean;
  // Bulk-archive control on the Completed group header. Off removes only that
  // affordance; per-session archive stays.
  archiveDoneButton: boolean;
  // Sticky strip at the top of a conversation showing its first user message
  // (the brief). Off hides the strip entirely.
  pinFirstMessage: boolean;
  // On a cold-cache session the composer offers a follow-up session (`offer`),
  // routes Enter to it (`default`), or stays silent (`off`).
  followupWhenCold?: FollowupWhenCold;
  // Follow-up takes the fork's spot in the drawer header.
  preferFollowupOverFork?: boolean;
  // Tint each conversation bubble's background with its role colour (assistant,
  // tool, user…) on top of the existing left rail, so a fast scroll reads
  // the flow by colour block rather than by badge. Off by default.
  roleTintedBackground: boolean;
  notifyEnabled: boolean;
  notifySound: boolean;
  // Where the route navigation lives on a wide screen: tabs inline in the
  // header, or the bottom bar. Below 48rem the bottom bar is always used.
  nav: NavPosition;
}

export const NAV_POSITIONS = ["top", "bottom"] as const;
export type NavPosition = (typeof NAV_POSITIONS)[number];
export const DEFAULT_NAV_POSITION: NavPosition = "top";

export function clampNavPosition(v: unknown): NavPosition {
  return (NAV_POSITIONS as readonly unknown[]).includes(v)
    ? (v as NavPosition)
    : DEFAULT_NAV_POSITION;
}

// The claude-code execution harness modes. Stored top-level in the settings
// blob as `data.harnessMode` because the server reads it from there
// (see settings.rs::harness_mode_of) to drive per-machine Reconcile. Codex
// sessions ignore this. An unknown stored value is clamped to `bg` server-side.
export type HarnessMode = "bg" | "sdk" | "oneshot";
export const HARNESS_MODES: readonly HarnessMode[] = ["bg", "sdk", "oneshot"];
export const DEFAULT_HARNESS_MODE: HarnessMode = "bg";

/** Clamp an arbitrary stored value to a known harness mode (mirrors the server's
 *  clamp so an unknown/missing value renders as `bg`). */
export function clampHarnessMode(v: unknown): HarnessMode {
  return HARNESS_MODES.includes(v as HarnessMode)
    ? (v as HarnessMode)
    : DEFAULT_HARNESS_MODE;
}

// Whip-mode stall-phrase override. Stored top-level as
// `data.whipStopPhrases` because the server clamps it there and serves it to the
// whip Stop hook. `extend` appends to the daemon's compiled defaults; `replace`
// swaps them out. Empty phrases + extend + no guidance is a no-op the server drops.
export type WhipMode = "extend" | "replace";
export const WHIP_MODES: readonly WhipMode[] = ["extend", "replace"];
export const DEFAULT_WHIP_MODE: WhipMode = "extend";

export interface WhipStopPhrases {
  mode: WhipMode;
  phrases: string[];
  guidance: string;
}

// A macro: a prompt plus the spawn knobs it runs with, launched from the
// Macros menu on the Sessions screen. `null` model / effort / pool /
// permission mode = the harness or account default. Serializes as
// `data.macros`; the server clamps shape and size but never runs one.
export interface MacroSpec {
  id: string;
  title: string;
  prompt: string;
  adapter: string;
  machine_id: string | null;
  working_dir: string | null;
  model: string | null;
  effort: string | null;
  pool_id: string | null;
  permission_mode: string | null;
  // Ask before spawning (default) or fire on a single click.
  confirm: boolean;
}

export interface MacrosSettings {
  enabled: boolean;
  items: MacroSpec[];
}

/** Clamp a stored value to a known whip mode (mirrors the server's clamp). */
export function clampWhipMode(v: unknown): WhipMode {
  return WHIP_MODES.includes(v as WhipMode)
    ? (v as WhipMode)
    : DEFAULT_WHIP_MODE;
}

// Daemon-side secret redaction. `secretScrubEnabled` toggles live
// scrubbing; `secretScrubPatterns` are extra user regexes layered on the daemon's
// compiled defaults. Stored top-level as `data.secretScrubEnabled` /
// `data.secretScrubPatterns`, which the server clamps (validates each regex,
// caps count/length) and syncs to the daemon via Reconcile.
export interface SecretScrubPattern {
  name: string;
  regex: string;
  enabled: boolean;
}

// Guided-tour state. Serializes as `data.onboarding` so a tour resumes on any
// device the user signs in from; the server stores the blob untouched.
export interface GuideStepProgress {
  /** Furthest step reached, 0-based. */
  index: number;
  /** Step count of that run, 0 when unknown. */
  total: number;
  version: number;
}

export interface OnboardingSettings {
  /** Journey id -> the version of it the user completed. */
  seenVersion: Record<string, number>;
  /** The runtime's serialized resume record for the tour in progress. */
  progress: string | null;
  /** Journey id -> how far a run ever got. The runtime drops `progress` when a
   *  run ends for any reason, so this is the only record of a half-finished
   *  tour. Absent in blobs written before it existed. */
  stepProgress: Record<string, GuideStepProgress>;
  /** Guides whose `DONE_PROBES` completion is ignored. A reset lists them all:
   *  the probes read live instance state, which a reset cannot undo, so without
   *  this they report done again immediately and the reset reads as a no-op. */
  probeOptOut: string[];
}

function mergeStepProgress(v: unknown): Record<string, GuideStepProgress> {
  const out: Record<string, GuideStepProgress> = {};
  if (!v || typeof v !== "object") return out;
  for (const [id, raw] of Object.entries(v as Record<string, unknown>)) {
    const r = raw as Partial<GuideStepProgress> | null;
    if (!r || typeof r.index !== "number" || !Number.isFinite(r.index))
      continue;
    out[id] = {
      index: Math.max(0, Math.floor(r.index)),
      total:
        typeof r.total === "number" && Number.isFinite(r.total)
          ? Math.max(0, Math.floor(r.total))
          : 0,
      version:
        typeof r.version === "number" && Number.isFinite(r.version)
          ? r.version
          : 1,
    };
  }
  return out;
}

export function mergeOnboarding(v: unknown): OnboardingSettings {
  const raw = (v ?? {}) as Partial<OnboardingSettings>;
  const seenVersion: Record<string, number> = {};
  if (raw.seenVersion && typeof raw.seenVersion === "object") {
    for (const [id, ver] of Object.entries(raw.seenVersion)) {
      if (typeof ver === "number" && Number.isFinite(ver))
        seenVersion[id] = ver;
    }
  }
  return {
    seenVersion,
    progress: typeof raw.progress === "string" ? raw.progress : null,
    stepProgress: mergeStepProgress(raw.stepProgress),
    probeOptOut: Array.isArray(raw.probeOptOut)
      ? [
          ...new Set(
            raw.probeOptOut.filter(
              (id): id is string => typeof id === "string",
            ),
          ),
        ]
      : [],
  };
}

/** Ratchet the furthest-reached step out of a resume record. The record carries
 *  the whole IR, so it also yields the run's step count. Never moves backwards
 *  within a version; a version bump restarts the count. */
export function ratchetStepProgress(
  prev: Record<string, GuideStepProgress>,
  progress: string | null,
): Record<string, GuideStepProgress> {
  if (!progress) return prev;
  let parsed: unknown;
  try {
    parsed = JSON.parse(progress);
  } catch {
    return prev;
  }
  const rec = parsed as
    | {
        id?: unknown;
        index?: unknown;
        version?: unknown;
        ir?: { steps?: unknown };
      }
    | null
    | undefined;
  const id = rec?.id;
  if (typeof id !== "string" || !id) return prev;
  const index =
    typeof rec?.index === "number" && Number.isFinite(rec.index)
      ? Math.max(0, Math.floor(rec.index))
      : 0;
  const version =
    typeof rec?.version === "number" && Number.isFinite(rec.version)
      ? rec.version
      : 1;
  const steps = rec?.ir?.steps;
  const total = Array.isArray(steps) ? steps.length : 0;
  const seen = prev[id];
  const fresh = !seen || seen.version !== version;
  if (!fresh && seen.index >= index && (total === 0 || seen.total === total))
    return prev;
  return {
    ...prev,
    [id]: {
      index: fresh ? index : Math.max(seen.index, index),
      total: total || (fresh ? 0 : seen.total),
      version,
    },
  };
}

export interface SettingsState {
  sessionList: SessionListSettings;
  display: DisplaySettings;
  // Docked spawn panel (Sessions screen). Top-level so it serializes as
  // `data.spawnDock`; the server passes it through untouched.
  spawnDock: SpawnDockSettings;
  // Docked stats panel (Sessions screen). Serializes as `data.statsDock`.
  statsDock: StatsDockSettings;
  // Header resource gauge machines. Serializes as `data.resourceMonitor`.
  resourceMonitor: ResourceMonitorSettings;
  // Claude harness mode. Top-level so it serializes as `data.harnessMode`,
  // which the server reads to drive each daemon's Reconcile.
  harnessMode: HarnessMode;
  // Whip-mode stall-phrase override. Top-level so it serializes as
  // `data.whipStopPhrases`, which the server clamps and feeds to the whip hook.
  whipStopPhrases: WhipStopPhrases;
  // Daemon-side secret redaction. Top-level so they serialize as
  // `data.secretScrubEnabled` / `data.secretScrubPatterns`, which the server
  // clamps and syncs to the daemon via Reconcile.
  secretScrubEnabled: boolean;
  secretScrubPatterns: SecretScrubPattern[];
  // Prefix auto-generated session names with an emoji. Top-level so it
  // serializes as `data.sessionEmojiPrefix`, which the server reads while it
  // persists the name the agent reported (cctui does not generate the name
  // itself). Off by default; a name the user typed is never decorated.
  sessionEmojiPrefix: boolean;
  // serializes as `data.autoResumeOnConnectionLoss`: the server nudges a
  // session stuck on an "API Error: Connection lost mid-response" with a
  // "continue" reply (1, 5 then 10 minutes) when this is on.
  autoResumeOnConnectionLoss: boolean;
  // Macros menu (off by default) and its entries. Serializes as `data.macros`.
  macros: MacrosSettings;
  // Which edge the toast stack sits on. Top-level so it serializes as
  // `data.toastPosition`; the server stores the blob untouched.
  toastPosition: ToastPosition;
  // Per-(machine, working-dir) spawn memory: the config last
  // submitted from the spawn modal, keyed by machineMemoryKey/dispatchMemoryKey
  // (spawnMemory.ts), LRU-capped. Replaces the localStorage per-machine prefs
  // so the memory follows the user across browsers.
  spawnMemory: SpawnMemoryMap;
  // Reserved for a future keyboard-shortcuts surface (no UI yet).
  shortcutsEnabled: boolean;
  keymap: Record<string, string>;
  // UI language. Top-level so it serializes as `data.locale`, which
  // the server clamps to en|fr|null. `null` means "auto" — fall back to the
  // browser's language / the base locale (Paraglide resolves it at runtime).
  locale: Locale | null;
  onboarding: OnboardingSettings;
}

const DEFAULTS: SettingsState = {
  sessionList: {
    sort: "activity",
    sortDir: DEFAULT_SORT_DIR,
    view: "list",
    density: "normal",
    section: "",
    labelFilter: [],
    colorBy: "none",
    groupBy: "status",
    width: DEFAULT_SESSION_LIST_WIDTH,
    accountNames: false,
  },
  display: {
    theme: "dark",
    fontScale: 1,
    archiveShortcut: true,
    archiveDoneButton: true,
    pinFirstMessage: true,
    roleTintedBackground: false,
    notifyEnabled: false,
    notifySound: true,
    nav: DEFAULT_NAV_POSITION,
  },
  spawnDock: { enabled: false, side: DEFAULT_SPAWN_DOCK_SIDE },
  statsDock: { enabled: false, side: DEFAULT_SPAWN_DOCK_SIDE },
  resourceMonitor: { machines: [] },
  harnessMode: DEFAULT_HARNESS_MODE,
  whipStopPhrases: { mode: DEFAULT_WHIP_MODE, phrases: [], guidance: "" },
  secretScrubEnabled: true,
  secretScrubPatterns: [],
  sessionEmojiPrefix: false,
  autoResumeOnConnectionLoss: false,
  macros: { enabled: false, items: [] },
  toastPosition: DEFAULT_TOAST_POSITION,
  spawnMemory: {},
  shortcutsEnabled: false,
  keymap: {},
  locale: null,
  onboarding: {
    seenVersion: {},
    progress: null,
    stepProgress: {},
    probeOptOut: [],
  },
};

// Deep-merge a partial saved blob over DEFAULTS so a value missing from an older
// payload (a field added in a later release) falls back to its default rather
// than becoming undefined. One level of nesting covers the catalogue shape.
// Stale keys in an older blob (e.g. a retired setting) are simply not copied
// over, and get pruned on the next save.
export function mergeDefaults(
  partial: Partial<SettingsState> | null | undefined,
): SettingsState {
  const p = partial ?? {};
  return {
    sessionList: {
      ...DEFAULTS.sessionList,
      ...(p.sessionList ?? {}),
      // Clamp so a stale/unknown stored value renders as the default column
      // width rather than an invalid CSS length.
      width: clampSessionListWidth(p.sessionList?.width),
      sortDir: clampSortDir(p.sessionList?.sortDir),
      groupBy: clampGroupBy(p.sessionList?.groupBy),
      accountNames: p.sessionList?.accountNames === true,
    },
    display: {
      ...DEFAULTS.display,
      ...(p.display ?? {}),
      archiveDoneButton: p.display?.archiveDoneButton !== false,
      pinFirstMessage: p.display?.pinFirstMessage !== false,
      followupWhenCold: clampFollowupWhenCold(p.display?.followupWhenCold),
      preferFollowupOverFork: p.display?.preferFollowupOverFork === true,
      roleTintedBackground: p.display?.roleTintedBackground === true,
      nav: clampNavPosition(p.display?.nav),
    },
    spawnDock: {
      enabled: p.spawnDock?.enabled === true,
      side: clampSpawnDockSide(p.spawnDock?.side),
      width: clampDockWidth(p.spawnDock?.width),
    },
    statsDock: {
      enabled: p.statsDock?.enabled === true,
      side: clampSpawnDockSide(p.statsDock?.side),
      width: clampDockWidth(p.statsDock?.width),
    },
    resourceMonitor: {
      machines: clampMonitoredMachines(p.resourceMonitor?.machines),
    },
    // Clamp to a known mode so an unknown stored value renders as `bg` (matches
    // the server's clamp on PUT).
    harnessMode: clampHarnessMode(p.harnessMode),
    whipStopPhrases: mergeWhipStopPhrases(p.whipStopPhrases),
    secretScrubEnabled: p.secretScrubEnabled !== false,
    secretScrubPatterns: mergeSecretScrubPatterns(p.secretScrubPatterns),
    sessionEmojiPrefix: p.sessionEmojiPrefix === true,
    autoResumeOnConnectionLoss: p.autoResumeOnConnectionLoss === true,
    macros: mergeMacros(p.macros),
    toastPosition: clampToastPosition(p.toastPosition),
    spawnMemory: p.spawnMemory ?? {},
    shortcutsEnabled: p.shortcutsEnabled ?? DEFAULTS.shortcutsEnabled,
    keymap: p.keymap ?? DEFAULTS.keymap,
    locale: clampLocale(p.locale),
    onboarding: mergeOnboarding(p.onboarding),
  };
}

// Coerce a stored whipStopPhrases value into the UI shape: clamp mode,
// keep only string phrases, coerce guidance to a string. The server drops a
// default block, so an absent value renders as the default.
function mergeWhipStopPhrases(
  v: Partial<WhipStopPhrases> | undefined,
): WhipStopPhrases {
  const raw = (v ?? {}) as Partial<WhipStopPhrases>;
  return {
    mode: clampWhipMode(raw.mode),
    phrases: Array.isArray(raw.phrases)
      ? raw.phrases.filter((p) => typeof p === "string")
      : [],
    guidance: typeof raw.guidance === "string" ? raw.guidance : "",
  };
}

// Coerce a stored secretScrubPatterns value into the UI shape: keep
// only well-formed `{ name, regex, enabled }` entries with a non-empty regex.
function mergeSecretScrubPatterns(v: unknown): SecretScrubPattern[] {
  if (!Array.isArray(v)) return [];
  return v
    .filter((e): e is Record<string, unknown> => !!e && typeof e === "object")
    .map((e) => ({
      name: typeof e.name === "string" ? e.name : "",
      regex: typeof e.regex === "string" ? e.regex : "",
      enabled: e.enabled !== false,
    }))
    .filter((e) => e.regex.trim().length > 0);
}

const optStr = (v: unknown): string | null =>
  typeof v === "string" && v.trim() ? v.trim() : null;

// Coerce a stored macros block into the UI shape: enabled only when literally
// true, and only items with an id, a title and a prompt survive.
function mergeMacros(v: unknown): MacrosSettings {
  const raw = (v ?? {}) as Partial<MacrosSettings>;
  const items = Array.isArray(raw.items)
    ? (raw.items as unknown[])
        .filter(
          (e): e is Record<string, unknown> => !!e && typeof e === "object",
        )
        .map((e): MacroSpec => ({
          id: typeof e.id === "string" ? e.id : "",
          title: typeof e.title === "string" ? e.title.trim() : "",
          prompt: typeof e.prompt === "string" ? e.prompt : "",
          adapter: optStr(e.adapter) ?? "claude-code",
          machine_id: optStr(e.machine_id),
          working_dir: optStr(e.working_dir),
          model: optStr(e.model),
          effort: optStr(e.effort),
          pool_id: optStr(e.pool_id),
          permission_mode: optStr(e.permission_mode),
          confirm: e.confirm !== false,
        }))
        .filter((e) => e.id && e.title && e.prompt.trim())
    : [];
  return { enabled: raw.enabled === true, items };
}

// Client-side payload migration chain, mirroring the server's idea: walk an
// older `data` blob up to CURRENT_VERSION. v1 is a passthrough — add a `case`
// per version bump. Pure; never throws.
function migrate(data: unknown, version: number): Partial<SettingsState> {
  const d = (data ?? {}) as Partial<SettingsState>;
  const v = version;
  // while (v < CURRENT_VERSION) { switch (v) { case 1: d = …; v = 2; break; } }
  void v;
  return d;
}

class Settings {
  state = $state<SettingsState>(mergeDefaults(null));

  private saveTimer: ReturnType<typeof setTimeout> | null = null;
  private loading: Promise<void> | null = null;
  // Save indicator for the Settings screen: `pending` while a debounced PUT is
  // queued or in flight, `saved` once the server acknowledged it (with the
  // time), `error` when the PUT failed (the local cache still holds the value).
  saveStatus = $state<"idle" | "pending" | "saved" | "error">("idle");
  savedAt = $state<number | null>(null);

  constructor() {
    if (browser) {
      // Synchronous seed from the localStorage cache for instant paint (also the
      // offline fallback). Tolerate corrupt/blocked storage — never throw during
      // module init (which would blank the whole UI).
      try {
        const raw = localStorage.getItem(KEY);
        if (raw)
          this.state = mergeDefaults(JSON.parse(raw) as Partial<SettingsState>);
      } catch {
        this.state = mergeDefaults(null);
      }
      const flush = () => this.flush();
      window.addEventListener("pagehide", flush);
      // Mobile browsers may never fire `pagehide` before killing the tab.
      document.addEventListener("visibilitychange", () => {
        if (document.visibilityState === "hidden") flush();
      });
    }
  }

  /** Pull the server copy once auth is known, run the migration chain, merge
   *  over defaults, and refresh the cache. Tolerates failure (401/offline) by
   *  keeping the cached/default state. Safe to call repeatedly; runs once. */
  load(): Promise<void> {
    if (!browser || !auth.isAuthed) return Promise.resolve();
    this.loading ??= this.fetchServerCopy();
    return this.loading;
  }

  private async fetchServerCopy(): Promise<void> {
    try {
      const payload = await api.get<SettingsPayload>("/settings");
      const migrated = migrate(
        payload.data,
        payload.version ?? CURRENT_VERSION,
      );
      this.state = mergeDefaults(migrated);
      this.writeCache();
      this.applyDisplay();
      if (this.state.locale) localeStore.set(this.state.locale);
    } catch {
      // 401 / offline / decode error — keep the cached or default state.
    }
  }

  private writeCache() {
    if (browser) {
      try {
        localStorage.setItem(KEY, JSON.stringify(this.state));
      } catch {
        /* quota / blocked storage — the in-memory state still holds it */
      }
    }
  }

  private sendSave(keepalive = false) {
    const body: SettingsPayload = {
      version: CURRENT_VERSION,
      data: this.state as unknown as SettingsPayload["data"],
    };
    // Fire-and-forget; the cache already holds the value if the PUT drops.
    void api
      .put("/settings", body, keepalive ? { keepalive: true } : undefined)
      .then(() => {
        // A later mutation re-armed the timer: stay pending for that one.
        if (this.saveTimer) return;
        this.saveStatus = "saved";
        this.savedAt = Date.now();
      })
      .catch(() => {
        if (!this.saveTimer) this.saveStatus = "error";
      });
  }

  private scheduleSave() {
    if (!browser || !auth.isAuthed) return;
    if (this.saveTimer) clearTimeout(this.saveTimer);
    this.saveStatus = "pending";
    this.saveTimer = setTimeout(() => {
      this.saveTimer = null;
      this.sendSave();
    }, SAVE_DEBOUNCE_MS);
  }

  /** Sends a queued PUT immediately. Without this, a reload inside the debounce
   *  window leaves the value cache-only and the next server fetch overwrites it. */
  flush() {
    if (!this.saveTimer) return;
    clearTimeout(this.saveTimer);
    this.saveTimer = null;
    this.sendSave(true);
  }

  /** Persist after a mutation: cache immediately, debounce the server PUT. */
  private persist() {
    this.writeCache();
    this.scheduleSave();
  }

  // Section setters — replace a whole group (or a subset of its fields) and
  // persist. Components mutate via these so every write goes through the cache +
  // debounced save path.
  setSessionList(patch: Partial<SessionListSettings>) {
    this.state.sessionList = { ...this.state.sessionList, ...patch };
    this.persist();
  }

  /** Column width of the session list, clamped on read so a blob written by a
   *  newer/older build can never feed a bogus length to the Container. */
  get sessionListWidth(): SessionListWidth {
    return clampSessionListWidth(this.state.sessionList.width);
  }

  /** Whether session rows spell the account name out instead of the key glyph. */
  get accountNames(): boolean {
    return this.state.sessionList.accountNames === true;
  }
  // Docked spawn panel: on/off and which edge it pins to.
  setSpawnDock(patch: Partial<SpawnDockSettings>) {
    this.state.spawnDock = { ...this.state.spawnDock, ...patch };
    this.persist();
  }

  get spawnDock(): SpawnDockSettings {
    return {
      enabled: this.state.spawnDock.enabled === true,
      side: clampSpawnDockSide(this.state.spawnDock.side),
      width: clampDockWidth(this.state.spawnDock.width),
    };
  }

  // Docked stats panel: on/off and which edge it pins to.
  /** Machines the header resource gauge shows, in the order they were ticked. */
  get monitoredMachines(): string[] {
    return clampMonitoredMachines(this.state.resourceMonitor?.machines);
  }

  setMonitoredMachine(machineId: string, on: boolean) {
    const cur = this.monitoredMachines;
    const next = on
      ? cur.includes(machineId)
        ? cur
        : [...cur, machineId]
      : cur.filter((id) => id !== machineId);
    if (next.length === cur.length && next.every((id, i) => id === cur[i]))
      return;
    this.state.resourceMonitor = { machines: next };
    this.persist();
  }

  setStatsDock(patch: Partial<StatsDockSettings>) {
    this.state.statsDock = { ...this.state.statsDock, ...patch };
    this.persist();
  }

  get statsDock(): StatsDockSettings {
    return {
      enabled: this.state.statsDock.enabled === true,
      side: clampSpawnDockSide(this.state.statsDock.side),
      width: clampDockWidth(this.state.statsDock.width),
    };
  }

  setDisplay(patch: Partial<DisplaySettings>) {
    this.state.display = { ...this.state.display, ...patch };
    this.persist();
  }

  // Display drivers routed through the blob so theme/font/notify round-trip
  // across devices: every surface (header + settings panel) mutates the runtime
  // singleton AND records the value here, and `load()` replays the blob back
  // into the singletons via `applyDisplay`.
  /** A picker choice: `auto`, or a theme id (which also becomes the memory of
   *  its light/dark slot). Persists the whole preference plus the resolved id. */
  setTheme(choice: string) {
    const p = themeMode.choose(choice);
    this.setDisplay({
      theme: themeMode.resolved,
      themeMode: p.mode,
      lightTheme: p.light,
      darkTheme: p.dark,
    });
  }

  setFontScaleLevel(levelId: string) {
    fontScale.set(levelId);
    this.setDisplay({ fontScale: fontScale.current });
  }

  setNotifySound(on: boolean) {
    notify.setSound(on);
    this.setDisplay({ notifySound: notify.sound });
  }

  /** Record the notifier's current enabled state after a header/panel toggle
   *  (whose async permission prompt the caller owns). */
  recordNotifyEnabled() {
    this.setDisplay({ notifyEnabled: notify.enabled });
  }

  private applyDisplay() {
    const d = this.state.display;
    themeMode.hydrate(preferenceFrom(d, themeMode.slotOf));
    fontScale.set(nearestLevel(d.fontScale));
    notify.applyPersisted(d.notifyEnabled, d.notifySound);
  }

  // Claude harness mode. Persisted top-level so it serializes as
  // `data.harnessMode`; the server clamps unknown values on PUT and pushes a
  // fresh Reconcile to the user's connected daemons within ~1s.
  setHarnessMode(mode: HarnessMode) {
    this.state.harnessMode = clampHarnessMode(mode);
    this.persist();
  }

  get harnessMode(): HarnessMode {
    return clampHarnessMode(this.state.harnessMode);
  }

  // Whip stall-phrase override. Persisted top-level; the server clamps
  // (trim/lowercase/dedupe/cap) and serves it to the whip Stop hook on next spawn.
  setWhipStopPhrases(patch: Partial<WhipStopPhrases>) {
    this.state.whipStopPhrases = { ...this.state.whipStopPhrases, ...patch };
    this.persist();
  }

  get whipStopPhrases(): WhipStopPhrases {
    return this.state.whipStopPhrases;
  }

  // Secret redaction. The server validates each regex on PUT and syncs
  // the effective list to the daemon via Reconcile.
  setSecretScrubEnabled(on: boolean) {
    this.state.secretScrubEnabled = on;
    this.persist();
  }

  setSecretScrubPatterns(patterns: SecretScrubPattern[]) {
    this.state.secretScrubPatterns = patterns;
    this.persist();
  }

  get secretScrubEnabled(): boolean {
    return this.state.secretScrubEnabled;
  }

  get secretScrubPatterns(): SecretScrubPattern[] {
    return this.state.secretScrubPatterns;
  }

  // Emoji prefix on agent-generated session names. Applied server-side when
  // the name lands, so it shows up everywhere the name does (list, cards,
  // notifications), not just in this browser.
  setSessionEmojiPrefix(on: boolean) {
    this.state.sessionEmojiPrefix = on;
    this.persist();
  }

  get sessionEmojiPrefix(): boolean {
    return this.state.sessionEmojiPrefix;
  }

  setAutoResumeOnConnectionLoss(on: boolean) {
    this.state.autoResumeOnConnectionLoss = on;
    this.persist();
  }
  get autoResumeOnConnectionLoss(): boolean {
    return this.state.autoResumeOnConnectionLoss;
  }

  get macrosEnabled(): boolean {
    return this.state.macros.enabled;
  }
  setMacrosEnabled(on: boolean) {
    this.state.macros = { ...this.state.macros, enabled: on };
    this.persist();
  }
  get macros(): MacroSpec[] {
    return this.state.macros.items;
  }
  /** Replace the whole list (add, edit, delete, reorder all go through here). */
  setMacros(items: MacroSpec[]) {
    this.state.macros = { ...this.state.macros, items };
    this.persist();
  }

  // Horizontal placement of the toast stack. Clamped on read so a blob written
  // by another build can never feed an unknown value to the `data-toast-pos`
  // attribute (which would leave the stack with no placement rule at all).
  setToastPosition(pos: ToastPosition) {
    this.state.toastPosition = pos;
    this.persist();
  }

  get toastPosition(): ToastPosition {
    return clampToastPosition(this.state.toastPosition);
  }

  // Spawn memory: write on spawn submit, recall on machine/cwd (or
  // dispatcher/repo) change in the spawn modal. Keys come from spawnMemory.ts.
  rememberSpawn(key: string, entry: Omit<SpawnMemoryEntry, "at">) {
    this.state.spawnMemory = putSpawnMemory(this.state.spawnMemory, key, {
      ...entry,
      at: Date.now(),
    });
    this.persist();
  }

  recallSpawn(key: string): SpawnMemoryEntry | null {
    return this.state.spawnMemory[key] ?? null;
  }

  /** The working dir most recently spawned on `machineId`, to pre-fill the cwd
   *  (which then keys the full recall). */
  lastDirFor(machineId: string): string | null {
    return latestDirFor(this.state.spawnMemory, machineId);
  }

  /** The machine's most recent entry regardless of dir, used when the picked
   *  cwd has no memory of its own. */
  lastEntryFor(machineId: string): SpawnMemoryEntry | null {
    return latestEntryFor(this.state.spawnMemory, machineId);
  }

  // UI language. Drives the Paraglide runtime immediately and persists
  // top-level as `data.locale` (server clamps to en|fr|null). `null` = auto.
  setLocale(next: Locale | null) {
    this.state.locale = clampLocale(next);
    if (this.state.locale) localeStore.set(this.state.locale);
    this.persist();
  }

  get locale(): Locale | null {
    return this.state.locale;
  }

  setNav(nav: NavPosition) {
    this.setDisplay({ nav: clampNavPosition(nav) });
  }

  get nav(): NavPosition {
    return clampNavPosition(this.state.display.nav);
  }

  // Guided-tour state, read and written by the journey runtime's storage
  // adapter (journey.ts). Kept opaque here: the runtime owns the shape.
  get onboarding(): OnboardingSettings {
    return mergeOnboarding(this.state.onboarding);
  }

  setOnboarding(patch: Partial<OnboardingSettings>) {
    const current = this.onboarding;
    const next = { ...current, ...patch };
    if (patch.progress !== undefined && patch.stepProgress === undefined) {
      next.stepProgress = ratchetStepProgress(
        current.stepProgress,
        patch.progress,
      );
    }
    this.state.onboarding = next;
    this.persist();
  }

  toggleArchiveShortcut() {
    this.setDisplay({ archiveShortcut: !this.state.display.archiveShortcut });
  }

  // Convenience reader for the most-used toggle (keeps call sites terse).
  get archiveShortcut(): boolean {
    return this.state.display.archiveShortcut;
  }

  get archiveDoneButton(): boolean {
    return this.state.display.archiveDoneButton;
  }

  setArchiveDoneButton(on: boolean) {
    this.setDisplay({ archiveDoneButton: on });
  }

  get pinFirstMessage(): boolean {
    return this.state.display.pinFirstMessage;
  }

  setPinFirstMessage(on: boolean) {
    this.setDisplay({ pinFirstMessage: on });
  }

  get followupWhenCold(): FollowupWhenCold {
    return clampFollowupWhenCold(this.state.display.followupWhenCold);
  }

  setFollowupWhenCold(v: FollowupWhenCold) {
    this.setDisplay({ followupWhenCold: v });
  }

  get preferFollowupOverFork(): boolean {
    return this.state.display.preferFollowupOverFork === true;
  }

  setPreferFollowupOverFork(on: boolean) {
    this.setDisplay({ preferFollowupOverFork: on });
  }

  get roleTintedBackground(): boolean {
    return this.state.display.roleTintedBackground;
  }

  setRoleTintedBackground(on: boolean) {
    this.setDisplay({ roleTintedBackground: on });
  }
}

export const settings = new Settings();
