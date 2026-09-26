export const PHONE_QUERY = '(max-width: 959px)';

/** Scale that fits a fixed-geometry terminal into `available` px; never enlarges. */
export function fitScale(available: number, natural: number): number {
	if (!(available > 0) || !(natural > 0)) return 1;
	return Math.min(1, available / natural);
}
