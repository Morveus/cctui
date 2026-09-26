import { expect, test, type Locator, type Page } from '@playwright/test';

type Box = { x: number; y: number; width: number; height: number };

const DESKTOP = { width: 1280, height: 800 };
const MOBILE = { width: 390, height: 844 };

const intersects = (a: Box, b: Box) =>
	a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height;

async function boxes(scope: Locator): Promise<{ label: string; box: Box }[]> {
	const out: { label: string; box: Box }[] = [];
	const n = await scope.count();
	for (let i = 0; i < n; i++) {
		const el = scope.nth(i);
		if (!(await el.isVisible())) continue;
		const box = await el.boundingBox();
		if (!box || box.width === 0 || box.height === 0) continue;
		out.push({ label: (await el.evaluate((e) => e.className.toString())) || `#${i}`, box });
	}
	return out;
}

async function openSessions(page: Page, viewport: { width: number; height: number }) {
	await page.setViewportSize(viewport);
	await page.goto('/sessions');
	await page.locator('header.hd .pill').waitFor();
	await page.waitForTimeout(500);
}

test('desktop: the version block clears every control in the tail (CCT-1013)', async ({ page }) => {
	await openSessions(page, DESKTOP);

	const vers = page.locator('header.hd .vers');
	await expect(vers).toBeVisible();
	const versBox = (await vers.boundingBox()) as Box;

	const neighbours = [
		...(await boxes(page.locator('header.hd .tail > *'))),
		...(await boxes(page.locator('header.hd .tail button, header.hd .tail a'))),
		...(await boxes(page.locator('header.hd .conn'))),
		...(await boxes(page.locator('header.hd .tabs a')))
	];
	expect(neighbours.length).toBeGreaterThan(0);

	console.log('vers', JSON.stringify(versBox));
	for (const n of neighbours) console.log('tail', n.label, JSON.stringify(n.box));

	const hits = neighbours.filter((n) => intersects(versBox, n.box));
	expect(hits.map((h) => h.label)).toEqual([]);

	await page.screenshot({
		path: `${process.env.HEADER_E2E_SHOTS}/desktop-1280.png`,
		clip: { x: 0, y: 0, width: DESKTOP.width, height: 80 }
	});
});

test('mobile: the version block yields instead of overlapping (CCT-1013)', async ({ page }) => {
	await openSessions(page, MOBILE);

	const vers = page.locator('header.hd .vers');
	await expect(vers).toBeHidden();

	const all = await boxes(page.locator('header.hd .vers'));
	const neighbours = await boxes(page.locator('header.hd .tail > *'));
	expect(neighbours.length).toBeGreaterThan(0);
	for (const v of all) for (const n of neighbours) expect(intersects(v.box, n.box)).toBe(false);

	await page.screenshot({
		path: `${process.env.HEADER_E2E_SHOTS}/mobile-390.png`,
		clip: { x: 0, y: 0, width: MOBILE.width, height: 80 }
	});
});

test('mobile: the versions hidden from the bar stay readable in Settings › Instance', async ({
	page
}) => {
	await page.setViewportSize(MOBILE);
	await page.goto('/settings/instance');
	const body = page.locator('body');
	await expect(body).toContainText(/srv \d+\.\d+\.\d+/);
	await expect(body).toContainText(/\bui (dev|\d+\.\d+\.\d+)/);
});

test('the system bar carries neither the ? nor a bell button (CCT-1012)', async ({ page }) => {
	await openSessions(page, DESKTOP);
	const tail = page.locator('header.hd .tail');
	await expect(tail.locator('a[href="/settings/guides"]')).toHaveCount(0);
	await expect(tail.getByText('?', { exact: true })).toHaveCount(0);
	await expect(tail.getByText('🔔')).toHaveCount(0);
	await expect(tail.getByText('🔕')).toHaveCount(0);
});

test('the user pill carries no notification bell at either width', async ({ page }) => {
	for (const viewport of [DESKTOP, MOBILE]) {
		await openSessions(page, viewport);
		await expect(page.locator('header.hd .pill .bell')).toHaveCount(0);
	}
});
