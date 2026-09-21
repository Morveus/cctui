import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vitest/config';

// Unit tests (CCT-338): the sveltekit() plugin resolves the `$lib` / `@bindings`
// aliases so the conversation formatting helpers import cleanly under vitest.
export default defineConfig({
	plugins: [sveltekit()],
	resolve: {
		conditions: ['browser'],
		alias: {
			$ghreview: new URL('../ghreview-ui/src', import.meta.url).pathname
		}
	},
	test: {
		environment: 'happy-dom',
		include: ['src/**/*.test.ts', 'scripts/**/*.test.mjs']
	}
});
