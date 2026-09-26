import { describe, expect, it } from 'vitest';
import {
	appendFileTokens,
	attachFiles,
	extForType,
	makeClipboardFiles,
	mergeFiles,
	mergeFilesRenamed,
	nextPasteIndex,
	rewriteFileTokens
} from './attachments';

const f = (name: string) => new File(['x'], name, { type: 'text/plain' });

describe('appendFileTokens', () => {
	it('appends a bracketed token per file to empty text', () => {
		expect(appendFileTokens('', [f('a.png'), f('b.csv')])).toBe('[a.png] [b.csv]');
	});

	it('separates from existing text with a space', () => {
		expect(appendFileTokens('see this:', [f('a.png')])).toBe('see this: [a.png]');
	});

	it('does not double a trailing space or newline', () => {
		expect(appendFileTokens('line one\n', [f('a.png')])).toBe('line one\n[a.png]');
		expect(appendFileTokens('word ', [f('a.png')])).toBe('word [a.png]');
	});

	it('skips names already referenced in the text', () => {
		expect(appendFileTokens('about [a.png] here', [f('a.png'), f('b.csv')])).toBe(
			'about [a.png] here [b.csv]'
		);
	});

	it('is idempotent on a re-pick of the same file', () => {
		const once = appendFileTokens('', [f('a.png')]);
		expect(appendFileTokens(once, [f('a.png')])).toBe(once);
	});
});

describe('mergeFiles', () => {
	it('keeps distinct names in order', () => {
		expect(mergeFiles([f('a.txt')], [f('b.txt')]).map((x) => x.name)).toEqual(['a.txt', 'b.txt']);
	});

	it('renames a duplicate instead of replacing it', () => {
		const { files, added } = mergeFilesRenamed([f('a.txt')], [f('a.txt')]);
		expect(files.map((x) => x.name)).toEqual(['a.txt', 'a-2.txt']);
		expect(added.map((x) => x.name)).toEqual(['a-2.txt']);
	});

	it('skips suffixes already taken, including within one batch', () => {
		const out = mergeFiles([f('a.txt'), f('a-2.txt')], [f('a.txt'), f('a.txt'), f('noext')]);
		expect(out.map((x) => x.name)).toEqual(['a.txt', 'a-2.txt', 'a-3.txt', 'a-4.txt', 'noext']);
		expect(mergeFiles([f('noext')], [f('noext')]).map((x) => x.name)).toEqual(['noext', 'noext-2']);
	});
});

describe('attachFiles', () => {
	it('rewrites the token to the renamed file', () => {
		const { files, text } = attachFiles([f('a.txt')], '[a.txt]', [f('a.txt')]);
		expect(files.map((x) => x.name)).toEqual(['a.txt', 'a-2.txt']);
		expect(text).toBe('[a.txt] [a-2.txt]');
	});
});

describe('nextPasteIndex', () => {
	const paste = (files: File[], text: string) => {
		const name = `paste-${nextPasteIndex(files, text)}.txt`;
		return attachFiles(files, text, [f(name)]);
	};

	it('numbers consecutive pastes paste-1, paste-2', () => {
		const a = paste([], '');
		const b = paste(a.files, a.text);
		expect(b.files.map((x) => x.name)).toEqual(['paste-1.txt', 'paste-2.txt']);
		expect(b.text).toBe('[paste-1.txt] [paste-2.txt]');
	});

	it('continues from tokens in the draft after a remount with no attachments', () => {
		expect(nextPasteIndex([], 'notes [paste-1.txt] more')).toBe(2);
		expect(paste([], '[paste-3.txt] [paste-1.txt]').text).toBe(
			'[paste-3.txt] [paste-1.txt] [paste-4.txt]'
		);
	});

	it('ignores non-paste names', () => {
		expect(nextPasteIndex([f('clipboard-7.png'), f('mypaste-2.txt')], '')).toBe(1);
	});

	it('skips names the session already staged', () => {
		expect(nextPasteIndex([], '', ['paste-1.txt'])).toBe(2);
		expect(nextPasteIndex([], '', ['paste-1.txt', 'paste-1-1.txt', 'shot.png'])).toBe(2);
		expect(nextPasteIndex([f('paste-4.txt')], '', ['paste-2.txt'])).toBe(5);
	});
});

describe('rewriteFileTokens', () => {
	it('points a token at the name staging returned', () => {
		const text = 'look\n\n[paste-1.txt]\n\nplease';
		const out = rewriteFileTokens(text, [f('paste-1.txt')], [
			'/tmp/cctui-uploads/s/paste-1-1.txt'
		]);
		expect(out).toBe('look\n\n[paste-1-1.txt]\n\nplease');
	});

	it('rewrites every occurrence and leaves unrenamed files alone', () => {
		const files = [f('paste-1.txt'), f('shot.png')];
		const paths = ['/tmp/cctui-uploads/s/paste-1-2.txt', '/tmp/cctui-uploads/s/shot.png'];
		expect(rewriteFileTokens('[paste-1.txt] a [shot.png] b [paste-1.txt]', files, paths)).toBe(
			'[paste-1-2.txt] a [shot.png] b [paste-1-2.txt]'
		);
	});

	it('leaves the text untouched when nothing was staged', () => {
		expect(rewriteFileTokens('[paste-1.txt]', [], [])).toBe('[paste-1.txt]');
	});
});

describe('extForType', () => {
	it('maps known MIME types', () => {
		expect(extForType('image/png')).toBe('png');
		expect(extForType('application/pdf')).toBe('pdf');
	});
	it('falls back to the sanitised subtype, then bin', () => {
		expect(extForType('text/x-log; charset=utf-8')).toBe('xlog');
		expect(extForType('')).toBe('bin');
	});
});

describe('makeClipboardFiles', () => {
	const item = (file: File | null, kind = 'file') =>
		({ kind, getAsFile: () => file }) as unknown as DataTransferItem;
	const dt = (items: DataTransferItem[], files: File[] = []) =>
		({ items, files }) as unknown as DataTransfer;

	it('names nameless blobs uniquely per surface', () => {
		const fromClipboard = makeClipboardFiles();
		const blob = new File(['x'], '', { type: 'image/png' });
		const a = fromClipboard(dt([item(blob)]));
		const b = fromClipboard(dt([item(blob)]));
		expect(a.map((f) => f.name)).toEqual(['clipboard-1.png']);
		expect(b.map((f) => f.name)).toEqual(['clipboard-2.png']);
	});

	it('keeps named files and ignores string items', () => {
		const fromClipboard = makeClipboardFiles();
		const named = new File(['x'], 'shot.png', { type: 'image/png' });
		const out = fromClipboard(dt([item(null, 'string'), item(named)]));
		expect(out).toEqual([named]);
	});

	it('falls back to .files when items carry no file', () => {
		const fromClipboard = makeClipboardFiles();
		const f = new File(['x'], 'a.txt', { type: 'text/plain' });
		expect(fromClipboard(dt([item(null, 'string')], [f]))).toEqual([f]);
	});
});
