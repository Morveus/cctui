<script lang="ts">
	import { CopyButton } from '@dorsk/tsumikit';
	import { statusDotClass, trimDetail, type DiagnoseBlockSummary } from '$lib/diagnoseRows';
	import { m } from '$lib/paraglide/messages';

	let { blocks }: { blocks: DiagnoseBlockSummary[] } = $props();
</script>

<ul class="blocks">
	{#each blocks as b (b.block)}
		<li class="block" data-block={b.block} data-status={b.status}>
			<div class="line">
				<span class="dot {statusDotClass(b.status)}"></span>
				<span class="title">{b.title}</span>
				<span class="short">{b.short}</span>
			</div>
			{#if b.status !== 'ok'}
				{#each b.rows.filter((r) => r.status !== 'ok' && r.detail) as r (r.label)}
					<div class="detail-row">
						<pre class="detail">{trimDetail(r.detail)}</pre>
						<CopyButton
							text={r.detail ?? ''}
							variant="ghost"
							box="xs"
							label={m.diagnose_copy_detail()}
						/>
					</div>
				{/each}
			{/if}
		</li>
	{/each}
</ul>

<style>
	.blocks {
		list-style: none;
		display: flex;
		flex-direction: column;
		gap: var(--sp-1);
		margin: 0;
		padding: 0;
	}
	.line {
		display: flex;
		align-items: center;
		gap: var(--sp-2);
	}
	.title {
		font-weight: 600;
		white-space: nowrap;
		color: var(--text);
	}
	.short {
		min-width: 0;
		overflow-wrap: anywhere;
		color: var(--text-muted);
	}
	.detail-row {
		display: flex;
		align-items: flex-start;
		gap: var(--sp-1);
		padding-left: var(--sp-3);
	}
	.detail {
		flex: 1;
		min-width: 0;
		margin: 0;
		white-space: pre-wrap;
		word-break: break-word;
		font-family: var(--font-mono, monospace);
		font-size: var(--fs-xs);
		color: var(--text-muted);
	}
</style>
