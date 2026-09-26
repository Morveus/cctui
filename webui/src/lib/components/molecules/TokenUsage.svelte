<script lang="ts">
	import type { TokenUsage } from '@bindings/TokenUsage';
	import { compact as compactNum, usd } from '$lib/format';
	import { Cluster, Text, Tooltip } from '@dorsk/tsumikit';
	import { m } from '$lib/paraglide/messages';
	import { bustReasonKey, tokenUsageLayout, tokenUsageTitle } from './TokenUsage.logic';

	// Canonical token-usage readout — the SINGLE token block, shared by the session
	// list/card, the chat header, and each assistant/result line in the conversation.
	// Renders:  Σtotal · ↑in ↓out ⚡cache · $cost (❄️)
	//   • Σ leads with the total (the headline number).
	//   • `sum` overrides Σ: the card passes the parent+subagents aggregate so Σ
	//     reflects the true cost-including-subagents; everywhere else Σ defaults to
	//     this block's OWN total (in + out + cache read + cache creation).
	//   • `cold`: the last turn re-billed the full context (prompt cache
	//     went cold) → a ❄️ next to the counts.
	//   • `showSum`: per-message conversation lines want only the reply's own
	//     breakdown (↑↓⚡), no leading Σ — they pass showSum={false}.
	//   • `wrap`: false (default) keeps the readout on one line — the list/header/
	//     lines never want it to break; the overview stats pass wrap so the larger
	//     `size` can fold inside a narrow card instead of spilling.
	//   • `compact`: forces the degraded Σtotal + $cost form. Inside a cramped
	//     `sess-card` / `drawer-head` size container the readout steps down on its
	//     own: Σ ↑↓⚡ $ → Σ $ → Σ (see the style block); the prop is the manual
	//     override. Σ always renders in the degraded forms, even with
	//     showSum={false}, and its tooltip always carries the full readout.
	// `size` rides through to each Text segment — defaults to the compact `xs`.
	//
	// The clarity hints render via the tsumikit Tooltip — no
	// native `title=`, so the bubble escapes overflow/transform clipping and reads
	// the same everywhere.
	let {
		usage,
		cold = false,
		sum = null,
		showSum = true,
		size = 'xs',
		wrap = false,
		compact = false
	}: {
		usage: TokenUsage;
		cold?: boolean;
		sum?: number | null;
		showSum?: boolean;
		size?: 'xs' | 'sm' | 'base' | 'md' | 'lg' | 'xl' | '2xl';
		wrap?: boolean;
		compact?: boolean;
	} = $props();

	const layout = $derived(tokenUsageLayout(usage, { sum, showSum, cold }));

	const sumHint = $derived(
		`${m.sessions_token_sum_hint()} — ${tokenUsageTitle(usage, layout, { num: compactNum, usd })}`
	);
	const costHint = m.sessions_token_cost_hint();
	const inHint = m.sessions_token_in_hint();
	const outHint = m.sessions_token_out_hint();
	const cacheHint = m.sessions_token_cache_hint();
	const coldHint = m.sessions_token_cold_hint();

	const bustReason = $derived.by(() => {
		switch (bustReasonKey(usage.cache_bust?.reason ?? '')) {
			case 'ttl_expired':
				return m.sessions_token_bust_reason_ttl_expired();
			case 'gateway_rewrote_body':
				return m.sessions_token_bust_reason_gateway_rewrote_body();
			default:
				return m.sessions_token_bust_reason_unknown();
		}
	});
	const bustHint = $derived(
		m.sessions_token_bust_hint({
			tokens: compactNum(Number(usage.cache_bust?.lost_tokens ?? 0)),
			cost: usd(Number(usage.cache_bust?.lost_usd ?? 0)),
			reason: bustReason
		})
	);
</script>

<!-- Cluster owns the layout (row, single gap, optional wrap); each segment is its
     own Text atom, carrying tone/weight/size as props rather than CSS overrides.
     Each hint is a tsumikit Tooltip wrapping its Text trigger. The `detail` /
     `sum-compact-only` spans are display:contents switches for the compact form. -->
<div class="usage" class:forced={compact}>
	<Cluster gap="0.4rem" align="baseline" {wrap}>
		{#if layout.sumMode !== 'never'}<span
				class:sum-compact-only={layout.sumMode === 'compact-only'}
				><Tooltip text={sumHint}>
					{#snippet trigger()}<Text
							variant="code"
							{size}
							tone="accent"
							weight="semibold"
							style="cursor:help">Σ{compactNum(layout.total)}</Text
						>{/snippet}
				</Tooltip></span
			>{/if}
		<span class="detail"
			><Tooltip text={inHint}>
				{#snippet trigger()}<Text variant="code" {size} tone="faint" style="cursor:help"
						>↑{compactNum(Number(usage.tokens_in))}</Text
					>{/snippet}
			</Tooltip>
			<Tooltip text={outHint}>
				{#snippet trigger()}<Text variant="code" {size} tone="faint" style="cursor:help"
						>↓{compactNum(Number(usage.tokens_out))}</Text
					>{/snippet}
			</Tooltip>
			{#if layout.showCache}<Tooltip text={cacheHint}>
					{#snippet trigger()}<Text variant="code" {size} tone="faint" style="cursor:help"
							>⚡{compactNum(layout.cacheTotal)}</Text
						>{/snippet}
				</Tooltip>{/if}</span
		>
		{#if layout.showCost}<span class="cost"
				><Tooltip text={costHint}>
					{#snippet trigger()}<Text
							variant="code"
							{size}
							tone="success"
							weight="semibold"
							style="cursor:help">{usd(layout.cost)}</Text
						>{/snippet}
				</Tooltip></span
			>{/if}
		{#if layout.showCold}<span class="detail"
				><Tooltip text={coldHint}>
					{#snippet trigger()}<Text
							variant="code"
							{size}
							tone="faint"
							style="font-size: 0.85em; cursor: help">❄️</Text
						>{/snippet}
				</Tooltip></span
			>{/if}
		{#if layout.showBust}<Tooltip text={bustHint}>
				{#snippet trigger()}<Text
						variant="code"
						{size}
						tone="danger"
						style="font-size: 0.85em; cursor: help">💥</Text
					>{/snippet}
			</Tooltip>{/if}
	</Cluster>
</div>

<style>
	.usage {
		flex: none;
		min-width: 0;
	}
	.detail,
	.cost,
	.sum-compact-only {
		display: contents;
	}
	.sum-compact-only,
	.forced .detail {
		display: none;
	}
	.forced .sum-compact-only {
		display: contents;
	}
	/* Degrade Σ ↑↓⚡ $ → Σ $ → Σ as the HOST gets cramped. The queries name the
	   ancestor size containers (`sess-card` on the card wrapper, `drawer-head` on
	   the chat header) rather than this block: the trailing footer groups are
	   flex:none, so the molecule's own inline size always equals its content size
	   and a self-query could never fire. They come last so they win the
	   equal-specificity tie against the defaults above. */
	@container sess-card (max-width: 26rem) {
		.detail {
			display: none;
		}
		.sum-compact-only {
			display: contents;
		}
	}
	@container sess-card (max-width: 16rem) {
		.cost {
			display: none;
		}
	}
	@container sess-row (max-width: 40rem) {
		.detail {
			display: none;
		}
		.sum-compact-only {
			display: contents;
		}
	}
	@container sess-row (max-width: 20rem) {
		.cost {
			display: none;
		}
	}
	@container drawer-head (max-width: 40rem) {
		.detail {
			display: none;
		}
		.cost {
			display: none;
		}
		.sum-compact-only {
			display: contents;
		}
	}
</style>
