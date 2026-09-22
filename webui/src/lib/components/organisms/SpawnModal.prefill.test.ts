import { mount, unmount } from "svelte";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import SpawnModal from "./SpawnModal.svelte";

const machineList = [
  {
    id: "m-uuid-1",
    name: "box",
    display_name: "box",
    kind: "persistent",
    hue: null,
  },
];
let recentDirsData: string[] = [];
let memoryDir: string | null = null;

vi.mock("$lib/queries", () => {
  const q = <T>(data: T) => ({ data, isLoading: false, isError: false });
  return {
    useAllMachines: () => q(machineList),
    useDispatchers: () => q([]),
    useSessions: () => q({ sessions: [] }),
    useRecentDirs: () => q(recentDirsData),
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
    lastDirFor: () => memoryDir,
    lastEntryFor: () => null,
    recallSpawn: () => null,
    rememberSpawn: () => {},
  },
}));

vi.mock("$lib/ws.svelte", () => ({ ws: { sessions: [] } }));

let component: ReturnType<typeof mount> | undefined;

beforeEach(() => {
  localStorage.clear();
  recentDirsData = [];
  memoryDir = null;
});
afterEach(async () => {
  if (component) await unmount(component);
  component = undefined;
  document.body.replaceChildren();
});

async function open() {
  component = mount(SpawnModal, {
    target: document.body,
    props: { onclose: () => {}, onspawned: () => {} },
  });
  await new Promise((r) => setTimeout(r, 100));
}

function cwdInput(): HTMLInputElement {
  const input = document.querySelector<HTMLInputElement>("#sp-cwd");
  if (!input) throw new Error("cwd field not found");
  return input;
}

function cwdValue(): string {
  return cwdInput().value;
}

describe("SpawnModal cwd prefill", () => {
  it("fills the cwd from the server recent dirs when spawn memory is empty", async () => {
    recentDirsData = ["/home/dorsk/Documents/cctui"];
    await open();
    expect(cwdValue()).toBe("/home/dorsk/Documents/cctui");
  });

  it("prefers the remembered dir over the recent dirs", async () => {
    recentDirsData = ["/srv/other"];
    memoryDir = "/home/dorsk/Documents/cctui";
    await open();
    expect(cwdValue()).toBe("/home/dorsk/Documents/cctui");
  });

  it("leaves the cwd empty when there is nothing to recall", async () => {
    await open();
    expect(cwdValue()).toBe("");
  });

  it("still takes a typed dir, and a cleared field, from the user", async () => {
    recentDirsData = ["/srv/other"];
    await open();
    expect(draftDir()).toBe("/srv/other");

    await type("/typed/by/hand");
    expect(draftDir()).toBe("/typed/by/hand");

    await type("");
    expect(draftDir()).toBe("");
  });
});

describe("SpawnModal cwd display", () => {
  it("shows the abbreviated path while blurred and the raw path while focused", async () => {
    recentDirsData = ["/home/dorsk/Documents/cctui"];
    await open();

    const overlay = () =>
      document
        .querySelector("#sp-cwd")
        ?.closest(".fi")
        ?.querySelector(".fi__display");

    // `.text` is the chosen candidate; WorkingDir's sibling width probe is not.
    const shown = () => overlay()?.querySelector(".text")?.textContent ?? "";

    expect(overlay()).not.toBeNull();
    expect(shown()).toMatch(/(^|\/)cctui$/);
    expect(cwdInput().value).toBe("/home/dorsk/Documents/cctui");

    cwdInput().focus();
    cwdInput().dispatchEvent(new FocusEvent("focus", { bubbles: true }));
    await settle();
    expect(overlay()).toBeNull();
    expect(cwdInput().value).toBe("/home/dorsk/Documents/cctui");

    cwdInput().dispatchEvent(new FocusEvent("blur", { bubbles: true }));
    await settle();
    expect(overlay()).not.toBeNull();
  });

  it("renders no overlay while the field is empty", async () => {
    await open();
    expect(document.querySelector(".fi__display")).toBeNull();
  });
});

function settle() {
  return new Promise((r) => setTimeout(r, 50));
}

function draftDir(): string {
  const raw = localStorage.getItem(
    localStorage.getItem("cctui_spawn_slot") ?? "cctui_spawn_draft",
  );
  return raw ? JSON.parse(raw).working_dir : "<no draft>";
}

async function type(value: string) {
  const input = cwdInput();
  input.focus();
  input.value = value;
  input.dispatchEvent(new Event("input", { bubbles: true }));
  await new Promise((r) => setTimeout(r, 50));
}
