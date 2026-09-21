// Capture the screens for some or all journeys, then compress them.
//
//   npm run journey:shoot -- --all
//   npm run journey:shoot -- --changed          # journeys your edits can affect
//   npm run journey:shoot -- sessions-list
//
// Needs a seeded local stack (`make local/demo`). One book pass runs per theme:
// the theme lives in the server's settings blob rather than in the browser, so
// it has to be re-seeded between passes and cannot vary within one.
import { execFileSync, spawn } from 'node:child_process';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { changedFiles, journeysForChanges, knownJourneys } from './journeys-for-changes.mjs';
import { previewStaleness } from './preview-freshness.mjs';
import { previewRenderFailure } from './preview-render.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const webui = resolve(here, '..');
const seed = resolve(webui, '../deploy/local/fixture/seed.sh');

const argv = process.argv.slice(2);
const token = process.env.CCTUI_TOKEN ?? 'dev-admin';
const env = { ...process.env, CCTUI_TOKEN: token, JOURNEY_VARS: JSON.stringify({ token }) };
const run = (cmd, args) => execFileSync(cmd, args, { cwd: webui, env, stdio: 'inherit' });

const compiled = JSON.parse(
	execFileSync('npx', ['journey', 'compile', '--public'], { cwd: webui, env, encoding: 'utf8' })
);

let ids;
if (argv.includes('--all')) ids = compiled.map((j) => j.id);
else if (argv.includes('--changed')) ids = journeysForChanges(changedFiles('origin/main'));
else ids = argv.filter((a) => !a.startsWith('--'));

if (!ids.length) {
	console.log('journey:shoot: nothing to capture');
	process.exit(0);
}

const unknown = ids.filter((id) => !knownJourneys().includes(id));
if (unknown.length) {
	console.error(`journey:shoot: unknown journey ${unknown.join(', ')}`);
	process.exit(1);
}

async function listening(url) {
	return await fetch(url, { redirect: 'follow' }).then(
		() => true,
		() => false
	);
}

/** Spawns `vite preview` on `url`'s port unless something already answers
 *  there, and resolves once it does. Returns the child to kill, or null when an
 *  existing server (already proven fresh above) is being reused. */
async function startPreview(url) {
	if (await listening(url)) return null;
	const port = new URL(url).port || '5273';
	const child = spawn('npx', ['vite', 'preview', '--port', port, '--strictPort'], {
		cwd: webui,
		env,
		stdio: 'ignore'
	});
	child.on('exit', (code) => {
		if (code) {
			console.error(`journey:shoot: vite preview exited with ${code}`);
			process.exit(1);
		}
	});
	const deadline = Date.now() + 120000;
	while (Date.now() < deadline) {
		if (await listening(url)) return child;
		await new Promise((r) => setTimeout(r, 250));
	}
	child.kill();
	console.error(`journey:shoot: vite preview never came up on ${url}`);
	process.exit(1);
}

const byTheme = new Map();
for (const id of ids) {
	const ir = compiled.find((j) => j.id === id);
	for (const theme of ir.variants?.theme ?? ['dark']) {
		byTheme.set(theme, [...(byTheme.get(theme) ?? []), id]);
	}
}

// Unconditional: `vite preview` serves whatever is on disk, so a server left
// running from an earlier build would otherwise capture stale UI.
run('npm', ['run', 'build']);

const appUrl = process.env.JOURNEY_APP_URL ?? 'http://localhost:5273';
const stale = await previewStaleness(appUrl);
if (stale) {
	console.error(`journey:shoot: the preview server at ${stale.url} cannot serve this build —`);
	console.error(`  ${stale.reason}.`);
	for (const m of stale.missing) console.error(`    ${m}`);
	console.error('  Stop it and re-run: a journey must document the build it was shot against.');
	process.exit(1);
}

// Started here rather than left to `journey book`, so the server is always
// younger than the build above and the render guard has something to probe.
const preview = await startPreview(appUrl);
process.on('exit', () => preview?.kill());
for (const sig of ['SIGINT', 'SIGTERM']) process.on(sig, () => process.exit(1));

const blank = await previewRenderFailure(appUrl);
if (blank) {
	console.error(`journey:shoot: ${appUrl} served a page that does not render —`);
	console.error(`  ${blank}.`);
	console.error('  Refusing to capture: a blank screenshot is a defect, not documentation.');
	process.exit(1);
}

run('node', [resolve(here, 'journey-auth.mjs')]);
for (const [theme, themeIds] of byTheme) {
	console.log(`\njourney:shoot: ${theme} — ${themeIds.join(', ')}`);
	run('bash', [seed, theme]);
	run('npx', ['journey', 'book', ...themeIds, '--variant', `theme=${theme}`]);
}
// Only what this run captured: compressing the whole record would rewrite
// flows that were never re-rendered and bury the real change in the diff.
run('node', [
	resolve(here, 'journey-compress.mjs'),
	...ids.map((id) => resolve(webui, '../docs/journeys', id))
]);
