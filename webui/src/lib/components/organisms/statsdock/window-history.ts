import type { UsageHistorySample, WastedSummary } from '$lib/queries';

export const WINDOW_MS: Record<string, number> = {
	session: 5 * 3600_000,
	weekly_all: 7 * 24 * 3600_000
};

/** How far back the chart reads, per window: enough for a handful of instances. */
export const LOOKBACK_MS: Record<string, number> = {
	session: 2 * 24 * 3600_000,
	weekly_all: 35 * 24 * 3600_000
};

export interface WindowLine {
	resetsAt: string;
	/** `x` is the elapsed fraction of the window (0..1), `y` the utilization. */
	points: { x: number; y: number }[];
}

/** Samples grouped into one line per window instance, oldest first. Instances
 *  whose `resets_at` differ by under a minute are the same window. */
export function windowLines(samples: UsageHistorySample[], windowKey: string): WindowLine[] {
	const len = WINDOW_MS[windowKey];
	if (!len) return [];
	const lines: { resetMs: number; line: WindowLine }[] = [];
	const sorted = samples
		.filter((s) => s.window_key === windowKey && s.resets_at)
		.sort((a, b) => Date.parse(a.sampled_at) - Date.parse(b.sampled_at));
	for (const s of sorted) {
		const resetMs = Date.parse(s.resets_at as string);
		let entry = lines.find((l) => Math.abs(l.resetMs - resetMs) < 60_000);
		if (!entry) {
			entry = { resetMs, line: { resetsAt: s.resets_at as string, points: [] } };
			lines.push(entry);
		}
		const x = 1 - (resetMs - Date.parse(s.sampled_at)) / len;
		entry.line.points.push({ x: Math.min(1, Math.max(0, x)), y: Math.max(0, s.utilization) });
	}
	return lines.sort((a, b) => a.resetMs - b.resetMs).map((l) => l.line);
}

/** SVG polyline points for a 100x100 viewBox, utilization capped at 100. */
export function polyline(line: WindowLine): string {
	return line.points
		.map((p) => `${(p.x * 100).toFixed(2)},${(100 - Math.min(100, p.y)).toFixed(2)}`)
		.join(' ');
}

/** Mean unused share of a window key, rounded, or null when none closed. */
export function wastedPct(summary: WastedSummary[] | undefined, windowKey: string): number | null {
	const s = summary?.find((x) => x.window_key === windowKey);
	return s && s.windows > 0 ? Math.round(s.mean_wasted_pct) : null;
}
