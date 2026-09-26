<script lang="ts">
	// Fork-conversation dialog. Forks the current conversation into a new
	// session, optionally changing model/effort — also the "reopen as a new
	// conversation" path for archived sessions and the supported "switch
	// model" substitute for claude (no in-place switch).
	import { compact } from '$lib/format';
	import { Button, Field, Modal, Select, Text, Textarea } from '@dorsk/tsumikit';
	import ModelPicker from '$lib/components/molecules/ModelPicker.svelte';
	import type { ModelOption } from '$lib/harnessModels';
	import { m } from '$lib/paraglide/messages';

	let {
		archived,
		isCodexSession,
		parentTokens,
		models,
		efforts,
		forking,
		extractLabel = null,
		model = $bindable(),
		effort = $bindable(),
		prompt = $bindable(''),
		oncancel,
		onsubmit
	}: {
		archived: boolean;
		isCodexSession: boolean;
		// Parent's total tokens — shown so the user knows the opening turn re-bills
		// this much context.
		parentTokens: number;
		models: ModelOption[];
		efforts: string[];
		forking: boolean;
		// Non-null → subset fork: the slice of the conversation to keep.
		extractLabel?: string | null;
		model: string;
		effort: string;
		prompt?: string;
		oncancel: () => void;
		onsubmit: () => void;
	} = $props();
</script>

<Modal
	title={extractLabel ? m.fork_title_extract() : archived ? m.fork_title_reopen() : m.fork_title()}
	size="sm"
	busy={forking}
	onclose={oncancel}
>
	{#snippet body()}
		<div class="fork-body">
			{#if extractLabel}
				<Text as="p" tone="accent" size="sm">{m.fork_extract_desc({ label: extractLabel })}</Text>
			{:else}
				<Text as="p" tone="muted" size="sm">
					{m.fork_desc({ target: isCodexSession ? m.fork_target_codex() : m.fork_target_claude() })}
				</Text>
				<Text as="p" tone="muted" size="sm">{m.fork_cost({ tokens: compact(parentTokens) })}</Text>
			{/if}
			<Field label={m.fork_model()} for="fork-model">
				<ModelPicker id="fork-model" bind:value={model} options={models} />
			</Field>
			<Field label={m.fork_effort()}>
				<Select bind:value={effort}>
					{#each efforts as e (e)}<option value={e}>{e || m.fork_default()}</option>{/each}
				</Select>
			</Field>
			<Field label={m.fork_prompt()} for="fork-prompt">
				<Textarea id="fork-prompt" rows={3} autoresize bind:value={prompt} placeholder={m.fork_prompt_placeholder()} />
			</Field>
		</div>
	{/snippet}
	{#snippet footer()}
		<Button onclick={oncancel} disabled={forking}>{m.common_cancel()}</Button>
		<Button variant="primary" onclick={onsubmit} disabled={forking}>
			{forking ? m.fork_forking() : archived ? m.fork_reopen() : m.fork_fork()}
		</Button>
	{/snippet}
</Modal>

<style>
	.fork-body {
		display: flex;
		flex-direction: column;
		gap: var(--sp-3);
	}
</style>
