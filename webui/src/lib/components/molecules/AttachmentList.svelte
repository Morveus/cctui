<script lang="ts">
	// Pending attachments for the spawn modal and the composer: the kit list
	// (tiles once `compact` and narrow) plus the upload cap error.
	import { fileCapError } from '$lib/attachments';
	import { AttachmentList } from '@dorsk/tsumikit';
	import Error from '$lib/components/atoms/Error.svelte';
	import { m } from '$lib/paraglide/messages';

	let {
		files,
		onremove,
		compact = false
	}: { files: File[]; onremove: (name: string) => void; compact?: boolean } = $props();

	const error = $derived(fileCapError(files));
</script>

{#if files.length}
	<AttachmentList
		{files}
		tiles={compact ? 'auto' : false}
		removeLabel={m.common_remove()}
		onremove={(i) => onremove(files[i].name)}
	/>
{/if}
{#if error}<Error>{error}</Error>{/if}
