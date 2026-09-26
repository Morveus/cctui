<script lang="ts">
	import type { SessionListItem } from '@bindings/SessionListItem';
	import { diagnoseBlocks, diagnoseRows } from '$lib/diagnoseRows';
	import DiagnoseBlocks from './DiagnoseBlocks.svelte';
	import { useSessionDiagnose } from '$lib/queries';

	// Mounted only once the dot's tooltip is armed: the query observer — and the
	// query client context it needs — must not exist for every dot in a list.
	let { session, now }: { session: SessionListItem; now: number } = $props();

	const report = useSessionDiagnose(() => session.id);
	const blocks = $derived(diagnoseBlocks(diagnoseRows(session, report.data ?? null, now)));
</script>

<DiagnoseBlocks {blocks} />
