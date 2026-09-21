// A served build whose assets all resolve can still paint nothing: the shell is
// static HTML, so a runtime that never boots yields a white frame the book will
// happily commit as documentation. The staleness probe cannot see that — it
// only fetches. This one runs the page.
import { chromium } from 'playwright';

/** Roots that mean the SPA reached a rendered state: the authed shell, or the
 *  login screen it falls back to. */
export const ROOT_SELECTOR = '.app, form';

const MIN_TEXT = 12;

/** Pure verdict over what the probe observed. `null` means the page rendered. */
export function blankPageReason({ rootFound, text, errors = [] }) {
	if (errors.length) return `the page raised ${errors[0]}`;
	if (!rootFound) return `no element matching \`${ROOT_SELECTOR}\` ever rendered`;
	const trimmed = (text ?? '').trim();
	if (trimmed.length < MIN_TEXT) {
		return `the root rendered but holds ${trimmed.length} characters of text`;
	}
	return null;
}

/** Loads `url` in a real browser and resolves to a reason string when the app
 *  did not paint, or `null` when it did. */
export async function previewRenderFailure(url, { timeout = 30000 } = {}) {
	const browser = await chromium.launch();
	try {
		const page = await browser.newPage();
		const errors = [];
		page.on('pageerror', (err) => errors.push(`${err.message}`));
		await page.goto(url, { waitUntil: 'load', timeout });

		let rootFound = true;
		await page.waitForSelector(ROOT_SELECTOR, { timeout }).catch(() => (rootFound = false));
		const text = rootFound ? await page.locator(ROOT_SELECTOR).first().innerText() : '';
		return blankPageReason({ rootFound, text, errors });
	} finally {
		await browser.close();
	}
}
