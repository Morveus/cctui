<script lang="ts">
	import { Avatar } from '@dorsk/tsumikit';

	// An account's identity mark: the owner's emoji when set, else a rounded
	// square tinted from the account id carrying the first letter of the name.
	let {
		emoji = null,
		name = '',
		id = '',
		size = 16,
		decorative = false
	}: {
		emoji?: string | null;
		name?: string | null;
		/** Account id — seeds the fallback colour, so it is stable everywhere. */
		id?: string;
		/** Box size in px; the glyph tracks it. */
		size?: number;
		/** The surrounding element already names the account: stay out of the
		 *  accessibility tree rather than repeating it. */
		decorative?: boolean;
	} = $props();

	const label = $derived(name ?? '');
	const glyph = $derived(emoji?.trim() ? emoji.trim() : undefined);
</script>

<Avatar
	name={label}
	{glyph}
	seed={id || label}
	tone={glyph ? 'none' : undefined}
	shape="square"
	{size}
	{decorative}
	title={decorative ? undefined : label}
/>
