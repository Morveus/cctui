import { describe, expect, it } from 'vitest';
import type { CodexModelCatalog } from '@bindings/CodexModelCatalog';
import {
	OTHER_MODEL,
	codexModels,
	codexModelsFor,
	declaredModelOptions,
	withDeclaredModels,
	customModelValue,
	preferCatalog,
	compareVersions,
	withCurrentModel
} from './harnessModels';

const catalog = (...ids: string[]): CodexModelCatalog => ({
	models: ids.map((id) => ({
		id,
		model: id,
		display_name: id.toUpperCase(),
		description: '',
		hidden: false,
		is_default: false,
		supported_efforts: [],
		default_effort: '',
		input_modalities: []
	}))
});

describe('customModelValue', () => {
	it('trims and treats blank as default', () => {
		expect(customModelValue('  gpt-6-astra ')).toBe('gpt-6-astra');
		expect(customModelValue('   ')).toBe('');
	});
});

describe('withCurrentModel', () => {
	it('lists an unknown current id as its own option', () => {
		// Deliberately an id the static list does not carry: the point is the
		// fallback path for a remembered/free-text model, not this id.
		expect(withCurrentModel(codexModels, 'gpt-nonesuch').at(-1)).toEqual({
			v: 'gpt-nonesuch',
			label: 'gpt-nonesuch'
		});
	});

	it('leaves the list alone for a known or empty value', () => {
		expect(withCurrentModel(codexModels, '')).toBe(codexModels);
		expect(withCurrentModel(codexModels, codexModels[0].v)).toBe(codexModels);
	});

	it('never mistakes the sentinel for a model', () => {
		expect(codexModels.some((o) => o.v === OTHER_MODEL)).toBe(false);
	});
});

describe('preferCatalog', () => {
	it('takes the first non-empty catalog', () => {
		const merged = catalog('gpt-b');
		expect(preferCatalog(undefined, { models: [] }, merged)).toBe(merged);
		expect(preferCatalog(undefined, undefined)).toBeUndefined();
	});

	it('drives the picker, static list only when nothing is live', () => {
		expect(codexModelsFor(preferCatalog(catalog('gpt-a'), catalog('gpt-b'))).map((o) => o.v)).toEqual(['', 'gpt-a']);
		expect(codexModelsFor(preferCatalog(undefined))).toBe(codexModels);
	});
});

describe('codexModels', () => {
	it('hardcodes no model slug', () => {
		expect(codexModels.map((o) => o.v)).toEqual(['']);
	});
});

describe('withDeclaredModels', () => {
	const claude = [
		{ v: '', label: 'Default' },
		{ v: 'opus', label: 'Opus' }
	];

	it('offers every declared model, then what the fallback adds', () => {
		const declared = [
			{ model: 'claude-opus-4-8', label: 'Opus 4.8' },
			{ model: 'claude-opus-5', label: 'Opus 5' }
		];
		expect(withDeclaredModels(declared, claude)).toEqual([
			{ v: '', label: 'Default' },
			{ v: 'claude-opus-4-8', label: 'Opus 4.8' },
			{ v: 'claude-opus-5', label: 'Opus 5' },
			{ v: 'opus', label: 'Opus' }
		]);
	});

	it('restores the plain list when the declared list is empty', () => {
		expect(withDeclaredModels([], claude)).toBe(claude);
		expect(withDeclaredModels(null, claude)).toBe(claude);
		expect(withDeclaredModels([{ model: '  ', label: 'blank row' }], claude)).toBe(claude);
	});

	it('never lists a declared id twice', () => {
		const out = withDeclaredModels([{ model: 'opus', label: 'Opus (pinned)' }], claude);
		expect(out.filter((o) => o.v === 'opus')).toEqual([{ v: 'opus', label: 'Opus (pinned)' }]);
	});
});

describe('declaredModelOptions', () => {
	it('falls back to the id as the label and drops half-filled rows', () => {
		expect(
			declaredModelOptions([
				{ model: ' claude-opus-5 ', label: '' },
				{ model: '', label: 'nothing' }
			])
		).toEqual([{ v: 'claude-opus-5', label: 'claude-opus-5' }]);
	});
});

describe('compareVersions', () => {
	it('orders numerically and ignores prerelease metadata', () => {
		expect(compareVersions('0.153.0', '0.156.1')).toBe(-1);
		expect(compareVersions('0.156.1', '0.153.0')).toBe(1);
		expect(compareVersions('0.156.1', '0.156.1')).toBe(0);
		expect(compareVersions('0.156.1-rc.1', '0.156.1')).toBe(0);
		expect(compareVersions('1.0', '1.0.0')).toBe(0);
		expect(compareVersions('nonsense', '0.1.0')).toBe(0);
	});
});

describe('codexModelsFor gating', () => {
	const gatedCatalog = (clientVersion?: string): CodexModelCatalog => ({
		client_version: clientVersion,
		models: [
			{ ...catalog('gpt-5.5').models[0] },
			{ ...catalog('gpt-6-astra').models[0], minimal_client_version: '0.153.0' },
			{ ...catalog('gpt-7').models[0], minimal_client_version: '0.999.0' },
			{ ...catalog('codex-auto-review').models[0], hidden: true }
		]
	});

	it('disables a model the catalog view cannot offer and hints the minimum', () => {
		const options = codexModelsFor(gatedCatalog('0.156.1'));
		expect(options.map((o) => o.v)).toEqual(['', 'gpt-5.5', 'gpt-6-astra', 'gpt-7']);
		expect(options[1].hint).toBeUndefined();
		expect(options[1].disabled).toBeUndefined();
		expect(options[2].disabled).toBe(false);
		expect(options[2].hint).toContain('0.153.0');
		expect(options[3].disabled).toBe(true);
		expect(options[3].hint).toContain('0.999.0');
	});

	it('hints without disabling when no client version is known', () => {
		const options = codexModelsFor(gatedCatalog());
		expect(options[3].disabled).toBe(false);
		expect(options[3].hint).toContain('0.999.0');
	});

	it('drops hidden models', () => {
		expect(codexModelsFor(gatedCatalog('0.156.1')).some((o) => o.v === 'codex-auto-review')).toBe(
			false
		);
	});
});
