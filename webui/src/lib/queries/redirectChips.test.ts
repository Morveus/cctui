import { describe, expect, it } from 'vitest';
import type { AccountRedirect } from '@bindings/AccountRedirect';
import { redirectChipsFor } from './accounts';

const rule = (o: Partial<AccountRedirect>): AccountRedirect =>
	({
		id: 'r1',
		from_account: 'a1',
		to_account: 'a2',
		family: 'anthropic',
		expires_at: null,
		...o
	}) as AccountRedirect;

const accounts = [
	{ id: 'a1', name: 'alpha' },
	{ id: 'a2', name: 'beta' }
];

describe('redirectChipsFor', () => {
	it('resolves the target uuid to its account name', () => {
		expect(redirectChipsFor([rule({ expires_at: 'later' })], accounts, 'a1')).toEqual([
			{ id: 'r1', family: 'anthropic', targetName: 'beta', until: 'later' }
		]);
	});

	it('falls back to an ellipsis when the target is not in the list', () => {
		expect(redirectChipsFor([rule({ to_account: 'gone' })], accounts, 'a1')[0].targetName).toBe('…');
	});

	it('drops rules that are not redirects and rules of other accounts', () => {
		expect(redirectChipsFor([rule({ to_account: null })], accounts, 'a1')).toEqual([]);
		expect(redirectChipsFor([rule({})], accounts, 'a2')).toEqual([]);
	});
});
