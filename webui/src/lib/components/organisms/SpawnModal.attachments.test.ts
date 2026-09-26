import { mount, unmount } from "svelte";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import SpawnModal from "./SpawnModal.svelte";
import { attachmentStore } from "$lib/attachmentStore";

const machineList = [
  {
    id: "m-uuid-1",
    name: "box",
    display_name: "box",
    kind: "persistent",
    hue: null,
  },
];

vi.mock("$lib/queries", () => {
  const q = <T>(data: T) => ({ data, isLoading: false, isError: false });
  return {
    useAllMachines: () => q(machineList),
    useDispatchers: () => q([]),
    useSessions: () => q({ sessions: [] }),
    useRecentDirs: () => q([]),
    useAccounts: () => q([]),
    useAccountPools: () => q([]),
    useLabels: () => q({ labels: [] }),
    useProfiles: () => q([]),
    useProfileActions: () => ({
      create: async () => ({ id: "p-1", name: "Default" }),
      update: async () => ({}),
      remove: async () => {},
    }),
    useAllAccountsUsage: () => q([]),
    useSessionActions: () => ({}),
    useCodexModels: () => q(null),
    useMergedCodexModels: () => q(null),
    useGitInfo: () => async () => ({ is_repo: false, is_worktree: false }),
    useMachineDirs: () => q([]),
    endpoints: { machineDirs: async () => [] },
  };
});

vi.mock("$lib/settings.svelte", () => ({
  settings: {
    state: { display: { archiveShortcut: true } },
    lastDirFor: () => null,
    lastEntryFor: () => null,
    recallSpawn: () => null,
    rememberSpawn: () => {},
  },
}));

vi.mock("$lib/ws.svelte", () => ({ ws: { sessions: [] } }));

const DRAFT = "cctui_spawn_draft";
const slot = () => localStorage.getItem("cctui_spawn_slot") ?? DRAFT;
const file = (name: string) =>
  new File(["hello"], name, { type: "text/plain" });

let component: ReturnType<typeof mount> | undefined;

beforeEach(async () => {
  localStorage.clear();
  await attachmentStore.clearAll();
});
afterEach(async () => {
  await close();
  document.body.replaceChildren();
});

const tick = (ms = 50) => new Promise((r) => setTimeout(r, ms));

async function open() {
  component = mount(SpawnModal, {
    target: document.body,
    props: { onclose: () => {}, onspawned: () => {} },
  });
  await tick(100);
}
async function close() {
  if (component) await unmount(component);
  component = undefined;
}

function must<T>(el: T | null | undefined, what: string): T {
  if (!el) throw new Error(`${what} not found`);
  return el;
}
const prompt = () =>
  must(document.querySelector<HTMLTextAreaElement>("#sp-prompt"), "prompt");
const chips = () =>
  [
    ...document.querySelectorAll('[data-tsu="AttachmentList"] li.chip [title]'),
  ].map((e) => e.getAttribute("title"));

async function pick(files: File[]) {
  const input = must(
    document.querySelector<HTMLInputElement>('input[type="file"]'),
    "file input",
  );
  Object.defineProperty(input, "files", { value: files, configurable: true });
  input.dispatchEvent(new Event("change", { bubbles: true }));
  await tick();
}

describe("SpawnModal attachment persistence", () => {
  it("keeps files and tokens across close and reopen", async () => {
    await open();
    prompt().value = "do it";
    prompt().dispatchEvent(new Event("input", { bubbles: true }));
    await tick();
    await pick([file("a.txt"), file("b.txt")]);
    expect(prompt().value).toBe("do it [a.txt] [b.txt]");
    expect(chips()).toEqual(["a.txt", "b.txt"]);
    await close();

    await open();
    expect(chips()).toEqual(["a.txt", "b.txt"]);
    expect(prompt().value).toBe("do it [a.txt] [b.txt]");
  });

  it("drops tokens whose file is missing", async () => {
    localStorage.setItem(
      DRAFT,
      JSON.stringify({ prompt: "read [a.txt] then [lost.txt] ok" }),
    );
    // An over-cap save records names only, so both files come back missing.
    await attachmentStore.set(DRAFT, [
      file("a.txt"),
      new File(["x".repeat(21 * 1024 * 1024)], "lost.txt"),
    ]);
    await open();
    expect(chips()).toEqual([]);
    expect(prompt().value).toBe("read then ok");
  });

  it("clears the store on Clear", async () => {
    await open();
    await pick([file("a.txt")]);
    expect((await attachmentStore.get(slot())).files.length).toBe(1);
    const clear = must(
      [...document.querySelectorAll<HTMLButtonElement>("button")].find(
        (b) => b.textContent?.trim() === "Clear",
      ),
      "Clear button",
    );
    clear.click();
    await tick();
    expect((await attachmentStore.get(slot())).files).toEqual([]);
  });
});
