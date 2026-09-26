import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';

// A raw colour in a component cannot follow the theme, and a `var(--x, raw)`
// fallback silently becomes the rendered value when `--x` is not a kit token —
// so the fallback, not the token, is what ships.
const ALLOWED = new Set([
	// A fixed preview swatch of one named theme, deliberately theme-independent.
	'src/lib/components/molecules/ThemeModePicker.svelte',
	// A mask gradient: the colour is an alpha channel, not a surface.
	'src/lib/components/organisms/conversation/ConversationLine.svelte'
]);

function walk(dir: string, out: string[] = []): string[] {
	for (const e of readdirSync(dir)) {
		const p = join(dir, e);
		if (statSync(p).isDirectory()) walk(p, out);
		else if (e.endsWith('.svelte')) out.push(p);
	}
	return out;
}

describe('component colours come from tokens', () => {
	it('no component carries a raw rgba()/hex value', () => {
		const root = join(process.cwd(), 'src');
		const offenders = walk(root)
			.map((f) => f.replace(`${root}`, 'src'))
			.filter((f) => !ALLOWED.has(f))
			.filter((f) => /rgba\(|#[0-9a-fA-F]{3,6}\b/.test(readFileSync(join(process.cwd(), f), 'utf8')));
		expect(offenders).toEqual([]);
	});
});
