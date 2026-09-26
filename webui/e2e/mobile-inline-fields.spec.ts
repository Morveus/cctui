import { expect, test, type Page } from '@playwright/test';

const MOBILE = { width: 390, height: 844 };

async function openSessions(page: Page) {
	await page.setViewportSize(MOBILE);
	await page.goto('/sessions');
	await page.locator('[data-journey="search"]').waitFor();
}

async function openDrawer(page: Page) {
	await openSessions(page);
	await page.locator('[data-journey="session"] [data-journey="title"]').first().click();
	await page.locator('[data-journey="composer"]').waitFor();
}

test('390px: the search box keeps its tools on the same row', async ({ page }) => {
	await openSessions(page);
	const search = await page.locator('[data-journey="search"]').boundingBox();
	const more = await page.locator('[data-journey="options"]').boundingBox();
	expect(search && more).toBeTruthy();
	expect(Math.abs(search!.y + search!.height / 2 - (more!.y + more!.height / 2))).toBeLessThan(8);
});

test('390px: the composer is one full-width field with attach and send inside it', async ({ page }) => {
	await openDrawer(page);
	const composer = page.locator('[data-journey="composer"]');
	const field = await composer.locator('textarea').boundingBox();
	const send = await composer.getByRole('button', { name: /send/i }).first().boundingBox();
	const inner = await page.evaluate(() => window.innerWidth);
	expect(field!.width).toBeGreaterThan(0.85 * inner);
	expect(send!.y).toBeGreaterThanOrEqual(field!.y - 1);
	expect(send!.y + send!.height).toBeLessThanOrEqual(field!.y + field!.height + 1);
});

test('390px: the drawer never scrolls sideways', async ({ page }) => {
	await openDrawer(page);
	const overflow = await page.evaluate(() => {
		const el = document.querySelector('.panel-content');
		return el ? el.scrollWidth - el.clientWidth : 0;
	});
	expect(overflow).toBeLessThanOrEqual(1);
});

test('390px: section headers keep sort and actions on the title row', async ({ page }) => {
	await openSessions(page);
	const heights = await page
		.locator('[data-tsu="SectionHeader"] .sh-row')
		.evaluateAll((rows) => rows.map((r) => r.getBoundingClientRect().height));
	expect(heights.length).toBeGreaterThan(0);
	for (const h of heights) expect(h).toBeLessThan(40);
});

test('390px: session rows never spill past the card', async ({ page }) => {
	await openSessions(page);
	const spill = await page
		.locator('[data-journey="session"]')
		.evaluateAll((cards) => cards.filter((c) => c.scrollWidth > c.clientWidth + 1).length);
	expect(spill).toBe(0);
});

test('390px: the drawer header keeps the title legible and its meta on one row', async ({ page }) => {
	await openDrawer(page);
	const head = page.locator('[data-journey="header"]');
	const title = await head.locator('.dtitle').boundingBox();
	const meta = await head.locator('[data-journey="head-meta"]').evaluate((el) => ({
		h: el.getBoundingClientRect().height,
		spill: el.scrollWidth - el.clientWidth
	}));
	expect(title!.width).toBeGreaterThan(120);
	expect(meta.h).toBeLessThan(40);
	expect(meta.spill).toBeLessThanOrEqual(1);
	await expect(head.getByTestId('permission-mode')).toHaveCount(0);
});

test('390px: the composer content sits right under its top edge', async ({ page }) => {
	await openDrawer(page);
	const gap = await page.locator('[data-journey="composer"]').evaluate((el) => {
		const first = [...el.children].find((c) => c.getBoundingClientRect().height > 0);
		return first ? first.getBoundingClientRect().top - el.getBoundingClientRect().top : Infinity;
	});
	expect(gap).toBeLessThanOrEqual(10);
});

test('390px: pending attachments fold into one row of tiles', async ({ page }) => {
	await openDrawer(page);
	const composer = page.locator('[data-journey="composer"]');
	const files = [1, 2, 3, 4].map((i) => ({
		name: `Screenshot 2026-09-25 at 11.3${i}.22.png`,
		mimeType: 'image/png',
		buffer: Buffer.alloc(9000 * i)
	}));
	await composer.locator('input[type=file]').first().setInputFiles(files);
	const tiles = composer.locator('li.tile');
	await expect(tiles).toHaveCount(4);
	const tops = await tiles.evaluateAll((els) => els.map((e) => Math.round(e.getBoundingClientRect().top)));
	expect(new Set(tops).size).toBe(1);
	await tiles.first().locator('button').click();
	await expect(page.getByRole('button', { name: /remove/i }).last()).toBeVisible();
});
