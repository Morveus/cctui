import { describe, expect, it } from 'vitest';
import { isValidAccountEmoji } from './avatar';

describe('isValidAccountEmoji', () => {
	it('accepts one emoji grapheme, including sequences and flags', () => {
		for (const ok of ['🐙', '❤️', '👍🏽', '👨‍👩‍👧', '🇫🇷', '☀', '']) {
			expect(isValidAccountEmoji(ok), ok).toBe(true);
		}
	});

	it('rejects text and multiple glyphs', () => {
		for (const bad of ['🐙🐙', 'hi', 'a', '🐙 🐙', '🇫🇷🇫🇷', '🐙‍', '🐙x', '🐙🐙🐙🐙🐙🐙🐙🐙🐙']) {
			expect(isValidAccountEmoji(bad), bad).toBe(false);
		}
	});
});
