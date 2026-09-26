const EMOJI_MAX_SCALARS = 8;
const isBase = (c: number) =>
	c === 0x203c ||
	c === 0x2049 ||
	c === 0x2122 ||
	c === 0x2139 ||
	(c >= 0x2190 && c <= 0x21ff) ||
	(c >= 0x2300 && c <= 0x23ff) ||
	(c >= 0x25aa && c <= 0x25ff) ||
	(c >= 0x2600 && c <= 0x27bf) ||
	c === 0x2934 ||
	c === 0x2935 ||
	(c >= 0x2b00 && c <= 0x2bff) ||
	c === 0x3030 ||
	c === 0x303d ||
	c === 0x3297 ||
	c === 0x3299 ||
	(c >= 0x1f000 && c <= 0x1faff);
const isModifier = (c: number) =>
	c === 0xfe0e ||
	c === 0xfe0f ||
	c === 0x20e3 ||
	(c >= 0x1f3fb && c <= 0x1f3ff) ||
	(c >= 0xe0020 && c <= 0xe007f);
const isRegional = (c: number) => c >= 0x1f1e6 && c <= 0x1f1ff;

/** Client-side mirror of the server's rule: one emoji grapheme, ZWJ sequences,
 *  skin tones and flags allowed. A blank value is valid — it clears the glyph
 *  back to the letter square. */
export function isValidAccountEmoji(value: string): boolean {
	const trimmed = (value ?? '').trim();
	if (!trimmed) return true;
	const points = [...trimmed].map((c) => c.codePointAt(0) ?? 0);
	if (points.length > EMOJI_MAX_SCALARS) return false;
	if (isRegional(points[0])) return points.length === 2 && isRegional(points[1]);
	let wantBase = true;
	for (const c of points) {
		if (wantBase) {
			if (!isBase(c)) return false;
			wantBase = false;
		} else if (c === 0x200d) {
			wantBase = true;
		} else if (!isModifier(c)) {
			return false;
		}
	}
	return !wantBase;
}
