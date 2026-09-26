import { defineConfig } from '@playwright/test';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const webui = resolve(dirname(fileURLToPath(import.meta.url)));

const port = Number(process.env.HEADER_E2E_PORT ?? 5291);
const headerUrl = process.env.HEADER_E2E_URL ?? `http://localhost:${port}`;

export default defineConfig({
	testDir: 'e2e',
	fullyParallel: false,
	workers: 1,
	reporter: [['list']],
	// Concurrent worktrees: a shared default port serves another branch's build.
	webServer: process.env.HEADER_E2E_URL
		? undefined
		: {
				command: `npx vite preview --port ${port} --strictPort`,
				url: headerUrl,
				cwd: webui,
				reuseExistingServer: false,
				timeout: 120_000,
				env: { CCTUI_PROXY: process.env.CCTUI_PROXY ?? 'http://localhost:8700' }
			},
	projects: [
		{
			name: 'header',
			testMatch: 'header-layout.spec.ts',
			use: { baseURL: headerUrl, storageState: resolve(webui, 'journeys/.auth/state.json') }
		},
		{
			name: 'mobile-fields',
			testMatch: 'mobile-inline-fields.spec.ts',
			use: { baseURL: headerUrl, storageState: resolve(webui, 'journeys/.auth/state.json') }
		},
		{
			name: 'spawn',
			testMatch: 'spawn-prompt-history.spec.ts',
			use: { baseURL: process.env.SPAWN_E2E_URL ?? 'http://localhost:5311' }
		}
	]
});
