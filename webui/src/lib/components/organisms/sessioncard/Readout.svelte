<script lang="ts">
	import AdapterIcon from '$lib/components/atoms/AdapterIcon.svelte';
	import TokenUsage from '$lib/components/molecules/TokenUsage.svelte';
	import { modelAbbrev, modelFamily, modelShort } from '$lib/format';
	import { Text } from '@dorsk/tsumikit';
	import type { SessionView } from './view';

	// Σ ↑ ↓ ⚡ $ · model · effort · adapter logo. As the container tightens the
	// model sheds its effort, then its version ("opus"), then all but two letters
	// ("Op."); at the last step the adapter logo goes too. The chip's tooltip
	// carries the full id at every step.
	let {
		view,
		compact = false,
		spread = false
	}: {
		view: SessionView;
		/** Σ + $ only (the compact row). */
		compact?: boolean;
		/** Push the model and logo to the far end of the line. */
		spread?: boolean;
	} = $props();

	const s = $derived(view.s);
	const modelTitle = $derived(
		s.model ? `${modelShort(s.model)}${s.effort ? ` · ${s.effort}` : ''}` : ''
	);
</script>

{#if !view.draft && s.status !== 'queued'}
	<TokenUsage usage={s.token_usage} cold={s.cache_cold} sum={view.rollup ? view.rollup.tokens : null} {compact} />
{/if}
{#if spread}<span class="gap"></span>{/if}
{#if s.model}
	<span class="model" title={modelTitle}>
		<Text tone="muted" size="xs" style="white-space:nowrap">
			<span class="full">{modelShort(s.model)}</span><span class="fam">{modelFamily(s.model)}</span
			><span class="abbr">{modelAbbrev(s.model)}</span
			>{#if s.effort}<span class="effort"> · {s.effort}</span>{/if}
		</Text>
	</span>
{/if}
<span class="logo"><AdapterIcon adapter={s.adapter_id} size={14} /></span>

<style>
	.model {
		flex: none;
		display: inline-flex;
		min-width: 0;
	}
	.logo {
		flex: none;
		display: inline-flex;
	}
	.gap {
		flex: 1 1 0;
	}
	.fam,
	.abbr {
		display: none;
	}
	@container sess-card (max-width: 26rem) {
		.effort {
			display: none;
		}
	}
	@container sess-card (max-width: 16rem) {
		.full {
			display: none;
		}
		.fam {
			display: inline;
		}
	}
	@container sess-card (max-width: 14rem) {
		.fam {
			display: none;
		}
		.abbr {
			display: inline;
		}
	}
	@container sess-row (max-width: 40rem) {
		.effort {
			display: none;
		}
	}
	@container sess-row (max-width: 34rem) {
		.full {
			display: none;
		}
		.fam {
			display: inline;
		}
	}
	@container sess-row (max-width: 20em) {
		.model {
			display: none;
		}
	}
	@container sess-row (max-width: 30rem) {
		.fam {
			display: none;
		}
		.abbr {
			display: inline;
		}
		.logo {
			display: none;
		}
	}
</style>
