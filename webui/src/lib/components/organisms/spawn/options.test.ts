import { describe, expect, it } from 'vitest';
import type { AccountProvider, OAuthAccount } from '$lib/queries';
import {
	accountAdapters,
	accountBacksAdapter,
	accountPickOptions,
	adapterForProvider,
	effectiveAdapterFor,
	NO_ACCOUNT,
	poolName,
	POOL_PREFIX,
	staleAccountPick,
	poolValue,
	providerForAdapter,
	contextPackEnv,
	CONTEXT_PACK_ENV,
	withAliasTargets
} from './options';

const provider = (p: string): AccountProvider => ({ provider: p, id: `id-${p}` }) as AccountProvider;
const account = (...providers: string[]): OAuthAccount =>
	({ name: 'acct', providers: providers.map(provider) }) as OAuthAccount;

describe('adapterForProvider', () => {
	it('maps the openai family to codex, everything else to claude-code', () => {
		expect(adapterForProvider('openai')).toBe('codex');
		expect(adapterForProvider('openai-compatible')).toBe('codex');
		expect(adapterForProvider('anthropic')).toBe('claude-code');
		expect(adapterForProvider('anthropic-compatible')).toBe('claude-code');
	});
});

describe('accountAdapters', () => {
	it('is the provider-family union, in stable order', () => {
		expect(accountAdapters(account('anthropic', 'openai'))).toEqual(['claude-code', 'codex']);
		expect(accountAdapters(account('openai', 'anthropic-compatible'))).toEqual([
			'claude-code',
			'codex'
		]);
		expect(accountAdapters(account('anthropic'))).toEqual(['claude-code']);
		expect(accountAdapters(account('openai-compatible'))).toEqual(['codex']);
		expect(accountAdapters(account())).toEqual([]);
	});
});

describe('providerForAdapter', () => {
	it('returns the credential backing the harness family', () => {
		const a = account('anthropic', 'openai');
		expect(providerForAdapter(a, 'claude-code')?.provider).toBe('anthropic');
		expect(providerForAdapter(a, 'codex')?.provider).toBe('openai');
	});
	it('is undefined without an account or a matching family', () => {
		expect(providerForAdapter(undefined, 'codex')).toBeUndefined();
		expect(providerForAdapter(account('anthropic'), 'codex')).toBeUndefined();
	});
});

describe('effectiveAdapterFor', () => {
	it('keeps the user pick with no account', () => {
		expect(effectiveAdapterFor(undefined, 'codex')).toBe('codex');
	});
	it('keeps the user pick when the account offers that family', () => {
		expect(effectiveAdapterFor(account('anthropic', 'openai'), 'codex')).toBe('codex');
	});
	it('falls back to the account first family when the pick is not offered', () => {
		expect(effectiveAdapterFor(account('anthropic'), 'codex')).toBe('claude-code');
		expect(effectiveAdapterFor(account('openai-compatible'), 'claude-code')).toBe('codex');
	});
});

describe('accountBacksAdapter', () => {
	// The submitted harness is the user's pick, gated by this predicate, NOT
	// rewritten by effectiveAdapterFor. A named anthropic-only account that
	// can't back codex blocks the spawn instead of silently submitting claude.
	it('is true with no account (Auto / No account runs any harness)', () => {
		expect(accountBacksAdapter(undefined, 'codex')).toBe(true);
		expect(accountBacksAdapter(undefined, 'claude-code')).toBe(true);
	});
	it('is true only when the account has a provider of that family', () => {
		expect(accountBacksAdapter(account('anthropic', 'openai'), 'codex')).toBe(true);
		expect(accountBacksAdapter(account('anthropic'), 'claude-code')).toBe(true);
	});
	it('is false when the account cannot back the picked harness', () => {
		// The exact regression: anthropic-only account + codex pick. The old
		// effectiveAdapterFor would coerce this to claude-code; the predicate now
		// rejects it so the submitted adapter_id is never silently swapped.
		expect(accountBacksAdapter(account('anthropic'), 'codex')).toBe(false);
		expect(effectiveAdapterFor(account('anthropic'), 'codex')).toBe('claude-code');
		expect(accountBacksAdapter(account('openai-compatible'), 'claude-code')).toBe(false);
	});
});

describe('withAliasTargets', () => {
	it('annotates aliased families and leaves the rest untouched', () => {
		const models = [
			{ v: '', label: 'Default' },
			{ v: 'opus', label: 'Opus' },
			{ v: 'sonnet', label: 'Sonnet' }
		];
		expect(withAliasTargets(models, { opus: 'claude-opus-4-8[1m]' })).toEqual([
			{ v: '', label: 'Default' },
			{ v: 'opus', label: 'Opus (claude-opus-4-8[1m])' },
			{ v: 'sonnet', label: 'Sonnet' }
		]);
		expect(withAliasTargets(models, null)).toEqual(models);
	});
});

describe('pool picker values', () => {
	it('round-trips a pool name through the sentinel', () => {
		expect(poolName(poolValue('personal'))).toBe('personal');
		// A pool named exactly like an account must stay distinguishable: the
		// value carries the kind, the name alone never does.
		expect(poolValue('acct')).not.toBe('acct');
	});

	it('reads every other picker value as not-a-pool', () => {
		expect(poolName('')).toBeUndefined();
		expect(poolName('acct')).toBeUndefined();
		expect(poolName(NO_ACCOUNT)).toBeUndefined();
	});
});

describe('contextPackEnv', () => {
	const pack = (over: Partial<Record<string, string>> = {}) => ({
		context_pack_url: '',
		context_pack_ref: '',
		context_pack_subdir: '',
		context_pack_token: '',
		...over
	});

	it('maps only the filled fields, trimmed', () => {
		expect(contextPackEnv(pack({ context_pack_url: '  https://x/y  ' }))).toEqual({
			CONTEXT_PACK_URL: 'https://x/y'
		});
	});

	it('is empty when nothing is set (blanks add no env)', () => {
		expect(contextPackEnv(pack({ context_pack_ref: '   ' }))).toEqual({});
	});

	it('overrides a duplicate raw env row when spread last', () => {
		const raw = { CONTEXT_PACK_URL: 'https://typo', OTHER: 'keep' };
		expect({ ...raw, ...contextPackEnv(pack({ context_pack_url: 'https://real' })) }).toEqual({
			CONTEXT_PACK_URL: 'https://real',
			OTHER: 'keep'
		});
	});

	it('emits keys the env-key pattern accepts', () => {
		for (const key of Object.values(CONTEXT_PACK_ENV)) expect(key).toMatch(/^[A-Z_][A-Z0-9_]*$/);
	});
});

describe('staleAccountPick', () => {
	const accounts = [account('anthropic')];

	it('flags a named account that no longer exists', () => {
		expect(staleAccountPick('gone', accounts)).toBe(true);
		expect(staleAccountPick('acct', accounts)).toBe(false);
	});

	it('never flags Auto, no-account, or a pool pick', () => {
		// A pool is neither Auto nor a named account: it must survive the
		// stale-account sweep or the picker snaps back to Auto on every click.
		expect(staleAccountPick('', accounts)).toBe(false);
		expect(staleAccountPick(NO_ACCOUNT, accounts)).toBe(false);
		expect(staleAccountPick(poolValue('personal'), accounts)).toBe(false);
		expect(staleAccountPick(poolValue('personal'), [])).toBe(false);
	});
});

describe('accountPickOptions', () => {
	const acct = (id: string, providers: string[] = ['anthropic']): OAuthAccount =>
		({ id, name: id, emoji: null, providers: providers.map(provider) }) as unknown as OAuthAccount;
	const build = (over: Partial<Parameters<typeof accountPickOptions>[0]> = {}) =>
		accountPickOptions({
			accounts: [acct('alpha'), acct('beta')],
			pools: [{ id: 'p1', name: 'work' }] as never,
			harness: 'claude-code',
			usedPct: (id) => (id === 'alpha' ? 100 : null),
			...over
		});

	it('sections pools, then accounts, then the sentinels', () => {
		expect(build().map((o) => o.group)).toEqual([
			'Pools',
			'Accounts',
			'Accounts',
			'Other',
			'Other'
		]);
	});

	it('keeps the value space unchanged, with Auto as the empty value', () => {
		expect(build().map((o) => o.value)).toEqual([
			`${POOL_PREFIX}p1`,
			'alpha',
			'beta',
			'',
			NO_ACCOUNT
		]);
	});

	it('keeps the usage hint on accounts and drops the per-row pool hint', () => {
		const opts = build();
		expect(opts.find((o) => o.value === 'alpha')?.hint).toBe('100%');
		expect(opts.find((o) => o.value === 'beta')?.hint).toBeUndefined();
		expect(opts[0].hint).toBeUndefined();
	});

	it('filters accounts that do not back the harness', () => {
		expect(build({ harness: 'codex' }).map((o) => o.value)).toEqual([
			`${POOL_PREFIX}p1`,
			'',
			NO_ACCOUNT
		]);
	});
});
