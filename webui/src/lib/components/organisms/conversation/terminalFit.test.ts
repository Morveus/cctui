import { describe, expect, it } from 'vitest';
import { fitScale } from './terminalFit';
import paneSource from './TerminalPane.svelte?raw';

describe('fitScale', () => {
	it('shrinks the 120-column frame into a phone-width pane', () => {
		expect(fitScale(390, 870)).toBeCloseTo(390 / 870);
	});

	it('never enlarges a frame that already fits', () => {
		expect(fitScale(1200, 870)).toBe(1);
	});

	it('leaves the frame alone until both sides are measured', () => {
		expect(fitScale(0, 870)).toBe(1);
		expect(fitScale(390, 0)).toBe(1);
		expect(fitScale(Number.NaN, 870)).toBe(1);
	});
});

describe('terminal pane on a phone', () => {
	it('keeps the daemon attach geometry fixed', () => {
		expect(paneSource).toContain('const COLS = 120;');
		expect(paneSource).toContain('const ROWS = 40;');
		expect(paneSource).not.toMatch(/\.resize\(/);
	});

	it('caps the pane height so the composer stays reachable', () => {
		expect(paneSource).toMatch(/max-height:\s*50svh/);
	});
});
