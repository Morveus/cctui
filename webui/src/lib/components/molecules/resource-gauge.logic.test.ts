import { describe, expect, it } from "vitest";
import type { MachineResourcesRow } from "@bindings/MachineResourcesRow";
import {
  fmtBytes,
  isStale,
  machineLabel,
  patchRow,
  pctOf,
  pinnedRows,
  resourceTone,
  STALE_AFTER_MS,
  worstTone,
} from "./resource-gauge.logic";

const res = (cpu: number, mem: number, disk: number) => ({
  cpu_pct: cpu,
  mem_pct: mem,
  mem_used_bytes: 0,
  mem_total_bytes: 0,
  disk_pct: disk,
  disk_used_bytes: 0,
  disk_total_bytes: 0,
  disk_path: "/home/x",
  load1: null,
});
const row = (
  id: string,
  r: ReturnType<typeof res> | null,
): MachineResourcesRow => ({
  machine_id: id,
  name: `host-${id}`,
  display_name: null,
  hue: null,
  liveness: "online",
  resources: r,
  updated_at: r ? "2026-09-06T10:00:00Z" : null,
  mem_ceiling_bytes: null,
});

describe("resourceTone", () => {
  it("is green under 70, orange from 70, red from 90, unknown when absent", () => {
    expect(resourceTone(0)).toBe("ok");
    expect(resourceTone(69.9)).toBe("ok");
    expect(resourceTone(70)).toBe("warn");
    expect(resourceTone(89.9)).toBe("warn");
    expect(resourceTone(90)).toBe("danger");
    expect(resourceTone(100)).toBe("danger");
    expect(resourceTone(null)).toBe("unknown");
    expect(resourceTone(Number.NaN)).toBe("unknown");
  });
});

describe("worstTone", () => {
  it("takes the worst of the three bars, unknown when nothing is known", () => {
    expect(worstTone(null)).toBe("unknown");
    expect(worstTone(res(10, 20, 30))).toBe("ok");
    expect(worstTone(res(10, 75, 30))).toBe("warn");
    expect(worstTone(res(95, 75, 30))).toBe("danger");
  });
});

describe("pctOf / fmtBytes / machineLabel", () => {
  it("rounds and clamps a percentage, null when unknown", () => {
    expect(pctOf(41.6)).toBe(42);
    expect(pctOf(140)).toBe(100);
    expect(pctOf(-3)).toBe(0);
    expect(pctOf(undefined)).toBeNull();
  });
  it("formats bytes for the tooltip", () => {
    expect(fmtBytes(512)).toBe("512 B");
    expect(fmtBytes(1536)).toBe("1.5 KB");
    expect(fmtBytes(16 * 1024 ** 3)).toBe("16 GB");
    expect(fmtBytes(-1)).toBe("?");
  });
  it("prefers the display name, falls back to the hostname", () => {
    expect(machineLabel({ name: "agents", display_name: null })).toBe("agents");
    expect(machineLabel({ name: "agents", display_name: "  " })).toBe("agents");
    expect(machineLabel({ name: "agents", display_name: "Agents VM" })).toBe(
      "Agents VM",
    );
  });
});

describe("pinnedRows / patchRow", () => {
  it("keeps the ticked order and drops ids that no longer exist", () => {
    const rows = [
      row("a", res(1, 1, 1)),
      row("b", null),
      row("c", res(2, 2, 2)),
    ];
    expect(
      pinnedRows(rows, ["c", "zzz", "a"]).map((r) => r.machine_id),
    ).toEqual(["c", "a"]);
    expect(pinnedRows(rows, [])).toEqual([]);
    expect(pinnedRows(undefined, ["a"])).toEqual([]);
  });
  it("patches only the named machine and marks it online", () => {
    const rows = [
      { ...row("a", null), liveness: "offline" as const },
      row("b", res(5, 5, 5)),
    ];
    const out = patchRow(rows, "a", res(50, 60, 70), "2026-09-06T11:00:00Z")!;
    expect(out[0].resources?.cpu_pct).toBe(50);
    expect(out[0].liveness).toBe("online");
    expect(out[0].updated_at).toBe("2026-09-06T11:00:00Z");
    expect(out[1]).toBe(rows[1]);
    expect(patchRow(undefined, "a", res(1, 1, 1), "x")).toBeUndefined();
  });
  it("keeps the RAM ceiling a live snapshot does not carry", () => {
    const rows = [{ ...row("a", null), mem_ceiling_bytes: 48 * 1024 ** 3 }];
    const out = patchRow(rows, "a", res(1, 1, 1), "2026-09-06T11:00:00Z");
    expect(out?.[0].mem_ceiling_bytes).toBe(48 * 1024 ** 3);
  });
});

describe("isStale", () => {
  it("hatches a snapshot older than the window, or never reported", () => {
    const now = Date.parse("2026-09-06T10:05:00Z");
    expect(isStale("2026-09-06T10:04:00Z", now)).toBe(false);
    expect(isStale(new Date(now - STALE_AFTER_MS - 1).toISOString(), now)).toBe(
      true,
    );
    expect(isStale(null, now)).toBe(true);
    expect(isStale("garbage", now)).toBe(true);
  });
});
