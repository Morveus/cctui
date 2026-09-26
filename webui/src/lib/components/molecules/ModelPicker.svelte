<script lang="ts">
	// Model select with a trailing "Other model…" entry that opens a free-text
	// id field. A value the option list doesn't know (typed or remembered) stays
	// selectable as its own option, so the picker never silently drops it.
	import type { ComponentProps } from 'svelte';
	import { Input, Select } from '@dorsk/tsumikit';
	import { OTHER_MODEL, customModelValue, withCurrentModel, type ModelOption } from '$lib/harnessModels';
	import { m } from '$lib/paraglide/messages';

	let {
		id,
		value = $bindable(''),
		options,
		compact = false,
		variant,
		width,
		'aria-label': ariaLabel
	}: {
		id: string;
		value: string;
		options: ModelOption[];
		compact?: boolean;
		variant?: ComponentProps<typeof Select>['variant'];
		width?: ComponentProps<typeof Select>['width'];
		'aria-label'?: string;
	} = $props();

	let otherPicked = $state(false);
	let text = $state('');
	// A value written from outside (memory recall, dialog reopen) closes the
	// free-text field so the select shows it.
	const custom = $derived(otherPicked && value === customModelValue(text));
	const listed = $derived(withCurrentModel(options, value));
	let selected = $derived(custom ? OTHER_MODEL : value);

	function pick(v: string) {
		if (v === OTHER_MODEL) {
			otherPicked = true;
			text = '';
			value = '';
			return;
		}
		otherPicked = false;
		value = v;
	}
</script>

<Select {id} {compact} {variant} {width} chevron={compact ? false : undefined} bind:value={selected} aria-label={ariaLabel} onchange={() => pick(selected)}>
	{#each listed as opt (opt.v)}<option value={opt.v} disabled={opt.disabled} title={opt.hint}
			>{opt.hint ? `${opt.label} — ${opt.hint}` : opt.label}</option
		>{/each}
	<option value={OTHER_MODEL}>{m.model_picker_other()}</option>
</Select>
{#if custom}
	<Input
		id="{id}-custom"
		mono
		size={compact ? 'sm' : 'md'}
		placeholder={m.model_picker_other_placeholder()}
		aria-label={m.model_picker_other_aria()}
		invalid={!customModelValue(text)}
		bind:value={text}
		oninput={() => (value = customModelValue(text))}
	/>
{/if}
