import type { SessionListItem } from '@bindings/SessionListItem';
import {
	BUCKETS,
	bucketInSection,
	groupId,
	groupOf,
	groupRows,
	matchesUnreadFilter,
	nest,
	queueOrder,
	rangeIds,
	sortSessions,
	type Dimension,
	type GroupKey,
	type Section,
	type SessionSort,
	type SessionSortDir,
	type SubGroup
} from './sessions.logic';

const bucketed = (dim: Dimension) => dim === 'none' || dim === 'status';

export interface SessionsListInputs {
	items: () => SessionListItem[];
	// Archived descendants of pinned parents, spliced back under their parent.
	pinnedArchivedKids: () => SessionListItem[];
	sections: () => Set<Section>;
	groupBy: () => Dimension;
	sort: () => SessionSort;
	sortDir: () => SessionSortDir;
	matchesLabel: (s: SessionListItem) => boolean;
	matchesClient: (s: SessionListItem) => boolean;
	// Visual document order of the rendered rows, for shift-range selection.
	renderedOrder: () => string[];
}

export class SessionsListController {
	#in: SessionsListInputs;

	selecting = $state(false);
	selected = $state(new Set<string>());
	anchorId = $state<string | null>(null);
	// Expand/collapse of collapsible (>=3) subagent groups, keyed by
	// `${parentId}/${group.key}`. Default collapsed.
	expanded = $state(new Set<string>());

	constructor(inputs: SessionsListInputs) {
		this.#in = inputs;
	}

	#sort = (rows: SessionListItem[]) => sortSessions(rows, this.#in.sort(), this.#in.sortDir());
	#keep = (s: SessionListItem): boolean =>
		this.#in.matchesLabel(s) &&
		this.#in.matchesClient(s) &&
		matchesUnreadFilter(s, this.#in.sections());

	draftRows = $derived.by(() => this.#in.items().filter((s) => s.status === 'draft'));
	// Spawns held back by their machine's RAM ceiling, in the order the reaper
	// will launch them. They ride with the live section, in their own group.
	queuedRows = $derived.by(() =>
		this.#in.sections().has('live')
			? queueOrder(this.#in.items().filter((s) => s.status === 'queued' && this.#keep(s)))
			: []
	);

	#liveNest = $derived.by(() =>
		nest([
			...this.#in.items().filter((s) => s.status !== 'draft' && s.status !== 'queued'),
			...this.#in.pinnedArchivedKids()
		])
	);
	get topLevel(): SessionListItem[] {
		return this.#liveNest.topLevel;
	}
	get childGroupsOf(): Map<string, SubGroup[]> {
		return this.#liveNest.childGroups;
	}

	groups = $derived.by(() =>
		BUCKETS.filter((b) => bucketInSection(b.key, this.#in.sections()))
			.map((b) => ({
				...b,
				sessions: this.#sort(
					this.#liveNest.topLevel.filter((s) => groupOf(s) === b.key && this.#keep(s))
				)
			}))
	);

	#liveTopFiltered = $derived.by(() =>
		this.#liveNest.topLevel.filter(
			(s) => bucketInSection(groupOf(s), this.#in.sections()) && this.#keep(s)
		)
	);
	groupedSections = $derived.by(() =>
		bucketed(this.#in.groupBy())
			? []
			: groupRows(this.#sort(this.#liveTopFiltered), this.#in.groupBy())
	);
	get hasLiveRows(): boolean {
		if (this.queuedRows.length > 0) return true;
		return bucketed(this.#in.groupBy())
			? this.groups.some((g) => g.sessions.length > 0)
			: this.groupedSections.length > 0;
	}


	toggleGroup = (parentId: string, key: string) => {
		const id = groupId(parentId, key);
		const next = new Set(this.expanded);
		if (next.has(id)) next.delete(id);
		else next.add(id);
		this.expanded = next;
	};
	isExpanded = (parentId: string, key: string): boolean => this.expanded.has(groupId(parentId, key));

	toggleSelect = (s: SessionListItem, range = false) => {
		const next = new Set(this.selected);
		if (range && this.anchorId && this.anchorId !== s.id) {
			const selectable = new Set(this.#in.items().map((x) => x.id));
			const ids = rangeIds(this.#in.renderedOrder(), this.anchorId, s.id, selectable);
			if (ids.length) {
				for (const id of ids) next.add(id);
				this.selected = next;
				return;
			}
		}
		if (next.has(s.id)) next.delete(s.id);
		else next.add(s.id);
		this.anchorId = s.id;
		this.selected = next;
	};
	exitSelect = () => {
		this.selecting = false;
		this.selected = new Set();
		this.anchorId = null;
	};
	selectAll = () => {
		this.selected = new Set(this.#in.items().map((s) => s.id));
	};

	bucketInSection = (key: GroupKey): boolean => bucketInSection(key, this.#in.sections());
}
