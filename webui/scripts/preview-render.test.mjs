import { describe, expect, it } from 'vitest';
import { blankPageReason, ROOT_SELECTOR } from './preview-render.mjs';

describe('blankPageReason', () => {
	it('passes a root that rendered real content', () => {
		expect(blankPageReason({ rootFound: true, text: 'Sessions  Accounts  Settings' })).toBeNull();
	});

	it('fails when the root never appeared', () => {
		const reason = blankPageReason({ rootFound: false, text: '' });
		expect(reason).toContain(ROOT_SELECTOR);
	});

	it('fails an empty root, which is what a stale preview paints', () => {
		expect(blankPageReason({ rootFound: true, text: '   ' })).toMatch(/0 characters/);
	});

	it('reports a runtime error ahead of the emptiness it caused', () => {
		const reason = blankPageReason({
			rootFound: false,
			text: '',
			errors: ['TypeError: failed to fetch module']
		});
		expect(reason).toContain('TypeError');
	});
});
