<script lang="ts">
	// Chips under a user bubble for the files that message uploaded: a
	// `paste-N.txt` mask expands inline, an image shows as a thumbnail, anything
	// else opens through the remote-file viewer (overlay or download).
	import { IconButton } from '@dorsk/tsumikit';
	import FileChip from '$lib/components/molecules/FileChip.svelte';
	import { attachmentStore } from '$lib/attachmentStore';
	import { copyText } from '$lib/clipboard';
	import { refusalMessage, tryOpenLocalFile } from '$lib/fileviewer';
	import { localFileHref } from '$lib/markdown';
	import { m } from '$lib/paraglide/messages';
	import { useSessionAttachments } from '$lib/queries';
	import { attachmentBlobUrl, pickAttachment, type SessionAttachment } from '$lib/queries/types';
	import { toasts } from '$lib/toast.svelte';
	import { isPasteName, type UserUploadRefs } from './lines';

	let {
		refs,
		ts,
		archived = false
	}: { refs: UserUploadRefs; ts: number; archived?: boolean } = $props();

	const query = useSessionAttachments(
		() => refs.sessionId ?? '',
		() => !!refs.sessionId && refs.names.length > 0
	);

	const resolved = $derived(
		refs.names
			.map((name) => pickAttachment(query.data ?? [], name, ts))
			.filter((a): a is SessionAttachment => a !== null)
	);

	const url = (a: SessionAttachment) => attachmentBlobUrl(a.session_id, a.hash);
	const isImage = (a: SessionAttachment) => (a.content_type ?? '').startsWith('image/');
	const isPaste = (a: SessionAttachment) => isPasteName(a.name);

	// A blob that 404s must not fall through to the browser's broken-image glyph:
	// the alt text then draws over it inside a line-height:0 box, which is the
	// unreadable overlapping chip users see.
	let brokenThumb = $state<Record<string, boolean>>({});

	let expanded = $state<Record<string, boolean>>({});
	let texts = $state<Record<string, string>>({});

	// The whole chain failed to produce this attachment: the chip goes inert and
	// says why, rather than re-toasting on every click.
	let gone = $state<Record<string, boolean>>({});

	// The daemon stages under `/tmp/cctui-uploads/<session>/<name>`, which is
	// also what the message's own `Attached file(s):` block printed.
	const stagedHref = (a: SessionAttachment): string | null =>
		a.machine_id && refs.sessionId
			? localFileHref(`/tmp/cctui-uploads/${refs.sessionId}/${a.name}`, {
					machineId: a.machine_id,
					sessionId: a.session_id
				})
			: null;

	/** Blob store → the staged copy on the machine. Resolves to the HTTP status
	 *  of the *blob* attempt, or `null` once something has served the file. */
	async function openWithFallback(a: SessionAttachment): Promise<number | null> {
		const blobStatus = await tryOpenLocalFile(url(a), a.name);
		if (blobStatus === null) return null;
		const href = stagedHref(a);
		if (href && (await tryOpenLocalFile(href, a.name)) === null) return null;
		return blobStatus;
	}

	async function openAttachment(a: SessionAttachment) {
		const status = await openWithFallback(a);
		if (status === null) return;
		if (status === 404) {
			gone[a.id] = true;
			toasts.error(m.conversation_attachment_gone({ name: a.name }));
			return;
		}
		toasts.error(refusalMessage(status, a.name, 'blob'));
	}

	async function fetchBody(a: SessionAttachment): Promise<Response | null> {
		const blob = await fetch(url(a), { credentials: 'same-origin' }).catch(() => null);
		if (blob?.ok) return blob;
		const href = stagedHref(a);
		if (!href) return null;
		const staged = await fetch(href, { credentials: 'same-origin' }).catch(() => null);
		return staged?.ok ? staged : null;
	}

	async function loadText(a: SessionAttachment): Promise<string | null> {
		if (texts[a.id] !== undefined) return texts[a.id];
		const cached = await attachmentStore.cachedText(a.session_id, a.hash);
		if (cached !== null) {
			texts[a.id] = cached;
			return cached;
		}
		const res = await fetchBody(a);
		if (!res) {
			gone[a.id] = true;
			toasts.error(m.conversation_attachment_load_failed({ name: a.name }));
			return null;
		}
		const text = await res.text();
		texts[a.id] = text;
		void attachmentStore.cacheText(a.session_id, a.hash, text);
		return text;
	}

	async function toggle(a: SessionAttachment) {
		if (!expanded[a.id] && (await loadText(a)) === null) return;
		expanded[a.id] = !expanded[a.id];
	}

	async function copy(a: SessionAttachment) {
		const text = await loadText(a);
		if (text !== null) await copyText(text);
	}

	const lineCount = (text: string) => text.split('\n').length;
</script>

{#if resolved.length}
	<div class="attachments">
		{#each resolved as a (a.id)}
			{#if isPaste(a)}
				<div class="paste">
					<div class="paste-head">
						<FileChip
							name={a.name}
							size={a.size}
							detail={texts[a.id] !== undefined
								? m.conversation_attachment_lines({ lines: lineCount(texts[a.id]) })
								: null}
							expanded={!!expanded[a.id]}
							unavailable={archived || gone[a.id]}
							title={gone[a.id]
								? m.conversation_attachment_unavailable({ name: a.name })
								: expanded[a.id]
									? m.conversation_attachment_collapse({ name: a.name })
									: m.conversation_attachment_expand({ name: a.name })}
							onclick={() => toggle(a)}
						/>
						{#if !archived}
							<IconButton
								inline
								icon="copy"
								label={m.conversation_attachment_copy({ name: a.name })}
								title={m.conversation_attachment_copy({ name: a.name })}
								onclick={() => copy(a)}
							/>
						{/if}
					</div>
					{#if expanded[a.id] && texts[a.id] !== undefined}
						<pre class="paste-body mono">{texts[a.id]}</pre>
					{/if}
				</div>
			{:else if isImage(a) && !brokenThumb[a.id]}
				<button
					type="button"
					class="thumb"
					title={m.conversation_attachment_open({ name: a.name })}
					onclick={() => openAttachment(a)}
				>
					<img
						src={url(a)}
						alt=""
						loading="lazy"
						onerror={() => (brokenThumb[a.id] = true)}
					/>
				</button>
			{:else}
				<FileChip
					name={a.name}
					size={a.size}
					unavailable={archived || gone[a.id]}
					title={gone[a.id]
						? m.conversation_attachment_unavailable({ name: a.name })
						: m.conversation_attachment_open({ name: a.name })}
					onclick={() => openAttachment(a)}
				/>
			{/if}
		{/each}
	</div>
{/if}

<style>
	.attachments {
		display: flex;
		flex-wrap: wrap;
		align-items: flex-start;
		gap: var(--sp-1) var(--sp-2);
		margin-top: 2px;
	}
	.paste {
		display: flex;
		flex-direction: column;
		min-width: 0;
		max-width: 100%;
	}
	.paste-head {
		display: inline-flex;
		align-items: center;
		gap: var(--sp-1);
	}
	.paste-body {
		margin: var(--sp-1) 0 0;
		padding: var(--sp-2);
		max-height: 22rem;
		overflow: auto;
		white-space: pre-wrap;
		font-size: calc(var(--fs-sm) - 0.0625rem);
		background: var(--bg-elevated-2);
		border: 1px solid var(--border);
		border-radius: var(--r-sm);
	}
	.thumb {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		min-width: 2rem;
		min-height: 2rem;
		padding: 0;
		background: none;
		border: 1px solid var(--border);
		border-radius: var(--r-sm);
		cursor: zoom-in;
		overflow: hidden;
	}
	.thumb img {
		display: block;
		max-width: 12rem;
		max-height: 8rem;
		object-fit: contain;
	}
</style>
