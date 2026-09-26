<script lang="ts" module>
	export const WARN_WINDOW_MS = 2 * 60 * 60 * 1000;
</script>

<script lang="ts">
	import type { SessionListItem } from '@bindings/SessionListItem';
	import { Button, Text } from '@dorsk/tsumikit';
	import { m } from '$lib/paraglide/messages';
	import { getLocale } from '$lib/paraglide/runtime';

	let {
		session,
		onpin
	}: {
		session: Pick<
			SessionListItem,
			'status' | 'liveness' | 'bucket' | 'auto_archive_at' | 'archived_by'
		>;
		onpin: () => void;
	} = $props();

	let now = $state(Date.now());
	$effect(() => {
		const t = setInterval(() => (now = Date.now()), 60_000);
		return () => clearInterval(t);
	});

	const archivedAutomatically = $derived(
		session.status === 'archived' && session.archived_by === 'automatic'
	);
	const dueIn = $derived.by(() => {
		if (session.status === 'archived' || session.liveness === 'active') return null;
		if (session.bucket === 'working' || session.bucket === 'blocked') return null;
		const at = session.auto_archive_at ? Date.parse(session.auto_archive_at) : Number.NaN;
		if (!Number.isFinite(at)) return null;
		const left = at - now;
		return left <= WARN_WINDOW_MS ? Math.max(0, left) : null;
	});
	const dueLabel = $derived.by(() => {
		if (dueIn === null) return '';
		const rtf = new Intl.RelativeTimeFormat(getLocale(), { numeric: 'auto' });
		const mins = Math.round(dueIn / 60_000);
		return mins < 60 ? rtf.format(mins, 'minute') : rtf.format(Math.round(mins / 60), 'hour');
	});
</script>

{#if archivedAutomatically}
	<div class="notice" role="note" data-testid="auto-archive-notice">
		<Text as="span" tone="faint" size="xs">{m.auto_archive_done()}</Text>
	</div>
{:else if dueIn !== null}
	<div class="notice" role="note" data-testid="auto-archive-notice">
		<Text as="span" tone="faint" size="xs">{m.auto_archive_due({ when: dueLabel })}</Text>
		<Button size="sm" variant="ghost" onclick={onpin}>{m.auto_archive_keep()}</Button>
	</div>
{/if}

<style>
	.notice {
		display: flex;
		align-items: center;
		gap: var(--sp-2);
		padding: 0 var(--sp-3);
		flex: none;
	}
</style>
