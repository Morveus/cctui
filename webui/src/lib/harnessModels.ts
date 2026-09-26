// Harness model + effort option lists. The server has no model allowlist;
// these strings pass through verbatim, and every picker accepts a free-text id.
import type { CodexModelCatalog } from '@bindings/CodexModelCatalog';
import { m as msg } from '$lib/paraglide/messages';

// Select sentinel for the free-text "Other model…" entry; never a real id.
export const OTHER_MODEL = '\u0000other';

export interface ModelOption {
	v: string;
	label: string;
	hint?: string;
	disabled?: boolean;
}

// Numeric semver compare, prerelease/build metadata ignored. Unparseable input
// compares equal so an odd version never disables a model.
export function compareVersions(a: string, b: string): number {
	const parts = (v: string) =>
		v
			.split(/[-+]/, 1)[0]
			.split('.')
			.map((n) => Number.parseInt(n, 10));
	const [x, y] = [parts(a), parts(b)];
	if (x.some(Number.isNaN) || y.some(Number.isNaN)) return 0;
	for (let i = 0; i < Math.max(x.length, y.length); i++) {
		const d = (x[i] ?? 0) - (y[i] ?? 0);
		if (d) return d < 0 ? -1 : 1;
	}
	return 0;
}

// Offline fallback for codex, used only when no catalog is known. No model slug
// is listed here on purpose: the server fetches the catalog per account, and
// free text covers a model no catalog has reached yet.
export const codexModels: ModelOption[] = [{ v: '', label: 'Default' }];
export const codexEfforts = ['', 'low', 'medium', 'high', 'xhigh', 'max', 'ultra'];

// Model options from a machine's live catalog, hidden models dropped and
// superseded ones (with an `upgrade`) suffixed. `Default` stays first so an
// unset model keeps codex's own pick. Empty/absent catalog → the static list.
export function codexModelsFor(catalog: CodexModelCatalog | undefined): ModelOption[] {
	const models = catalog?.models ?? [];
	if (!models.length) return codexModels;
	const options: ModelOption[] = [{ v: '', label: 'Default' }];
	const current = catalog?.client_version ?? '';
	for (const model of models) {
		if (model.hidden) continue;
		const label = model.upgrade ? `${model.display_name} (superseded)` : model.display_name;
		const min = model.minimal_client_version ?? '';
		if (!min) {
			options.push({ v: model.id, label });
			continue;
		}
		const gated = !!current && compareVersions(min, current) > 0;
		options.push({
			v: model.id,
			label,
			hint: gated
				? msg.codex_model_gated({ version: min, current })
				: msg.codex_model_needs_version({ version: min }),
			disabled: gated
		});
	}
	return options;
}

// Effort levels a given model supports, `''` (default) first so the
// picker can leave codex its own default. An unknown model or empty catalog
// falls back to the full static effort list.
export function codexEffortsFor(
	catalog: CodexModelCatalog | undefined,
	modelId: string
): string[] {
	const models = catalog?.models ?? [];
	if (!models.length) return codexEfforts;
	const model = modelId ? models.find((m) => m.id === modelId) : models.find((m) => m.is_default);
	const supported = model?.supported_efforts ?? [];
	if (!supported.length) return codexEfforts;
	return ['', ...supported];
}

export const claudeModels: ModelOption[] = [
	{ v: '', label: 'Default' },
	{ v: 'haiku', label: 'Haiku' },
	{ v: 'sonnet', label: 'Sonnet' },
	{ v: 'opus', label: 'Opus' },
	{ v: 'fable', label: 'Fable' }
];
export const claudeEfforts = ['', 'low', 'medium', 'high', 'xhigh', 'max'];

// The models a provider declares, in the order the operator listed them; a row
// with no id is a half-filled editor row and is dropped.
export function declaredModelOptions(
	models: { model: string; label: string }[] | null | undefined
): ModelOption[] {
	return (models ?? [])
		.filter((mo) => mo.model.trim())
		.map((mo) => ({ v: mo.model.trim(), label: mo.label.trim() || mo.model.trim() }));
}

// Declared models first, then whatever the catalog/native list adds that they
// don't already cover, so an operator's curated set leads the picker without
// hiding the rest.
export function withDeclaredModels(
	models: { model: string; label: string }[] | null | undefined,
	fallback: ModelOption[]
): ModelOption[] {
	const declared = declaredModelOptions(models);
	if (!declared.length) return fallback;
	const seen = new Set(declared.map((o) => o.v));
	return [
		...(fallback.some((o) => o.v === '') ? [{ v: '', label: 'Default' }] : []),
		...declared,
		...fallback.filter((o) => o.v && !seen.has(o.v))
	];
}

// Reads a free-text model id: whitespace-trimmed, empty meaning "Default".
export function customModelValue(text: string): string {
	return text.trim();
}

// Keeps a value the option list doesn't know (a free-text or remembered id)
// selectable by listing it as its own option.
export function withCurrentModel(options: ModelOption[], current: string): ModelOption[] {
	if (!current || options.some((o) => o.v === current)) return options;
	return [...options, { v: current, label: current }];
}

// The live catalog to drive a codex picker with: the machine's own when it
// has one, else the cross-machine merge, else nothing (static fallback).
export function preferCatalog(
	...catalogs: (CodexModelCatalog | undefined)[]
): CodexModelCatalog | undefined {
	return catalogs.find((c) => c?.models.length);
}
