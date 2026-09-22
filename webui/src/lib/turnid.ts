/** Identity of one human turn, minted when the user hits send and carried
 * through the daemon onto every event Claude derives from that turn. Matching
 * turns by this id is what lets the drawer collapse the several text encodings
 * of one turn without having to prove they are the same content.
 *
 * UUIDv7 rather than v4 so the ids sort by send time, which keeps them useful
 * as a debugging trail and as a DB index key. */
export function newTurnId(): string {
	const bytes = new Uint8Array(16);
	crypto.getRandomValues(bytes);
	const ts = Date.now();
	for (let i = 0; i < 6; i++) bytes[i] = Math.floor(ts / 2 ** (8 * (5 - i))) & 0xff;
	bytes[6] = (bytes[6] & 0x0f) | 0x70;
	bytes[8] = (bytes[8] & 0x3f) | 0x80;
	const hex = [...bytes].map((b) => b.toString(16).padStart(2, '0')).join('');
	return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}
