/**
 * Export a conversation as a single self-contained HTML document.
 *
 * Built entirely client-side from the merged event list the drawer already
 * holds — no server round-trip. The file embeds all CSS so the browser's
 * Print → "Save as PDF" yields a clean PDF: that IS the PDF path; we don't
 * ship a PDF generator.
 *
 * The export mirrors the live view (follow-up to the first cut):
 *  - THEME: the active theme's palette tokens (dark/light/sepia) are read
 *    from the document's computed styles at export time and baked into the
 *    file, so a sepia screen exports a sepia transcript.
 *  - FILTERS: the drawer's message-category checkboxes and the JSON / Diff
 *    prettification toggles gate the export the same way they gate the
 *    on-screen lines — what you see is what you save.
 */

import type { AgentEvent } from "@bindings/AgentEvent";
import type { SessionListItem } from "@bindings/SessionListItem";
import type {
  MsgCategory,
  MsgFilter,
} from "$lib/components/organisms/conversation/types";
import {
  looksPoll,
  normalizePollText,
  resultCategory,
} from "$lib/components/organisms/conversation/lines";
import { looksMeta } from "$lib/components/organisms/conversation/format";
import {
  renderMarkdown,
  highlightBlock,
  prettyJson,
  escapeHtml,
} from "$lib/markdown";
import { USER_PREFIX } from "$lib/ws.svelte";
import { parsePeerMessage } from "$lib/components/organisms/conversation/format";
import { getLocale } from "$lib/paraglide/runtime";

/** The subset of the drawer's ViewOpts the export honors. Typed from the
 * drawer's own categories so a new filter cannot silently skip the export. */
export interface ExportOpts {
  msgFilter: MsgFilter;
  prettyJson: boolean;
  prettyDiff: boolean;
  prettyTables: boolean;
}

const visible = (opts: ExportOpts, c: MsgCategory): boolean =>
  opts.msgFilter[c] !== false;

const pollRole = (
  content: string,
  system: boolean,
  seen: Set<string>,
): "poll" | "system" | "user" => {
  if (looksPoll(content)) return "poll";
  if (system) return "system";
  const norm = normalizePollText(content);
  if (!norm) return "user";
  if (seen.has(norm)) return "poll";
  seen.add(norm);
  return "user";
};

interface Block {
  role:
    | "assistant"
    | "thinking"
    | "user"
    | "poll"
    | "peer"
    | "system"
    | "marker"
    | "tool"
    | "result"
    | "reset"
    | "compact"
    | "summary"
    | "ask";
  ts: number;
  label?: string; // tool name / divider text
  html: string; // inner HTML, already escaped/rendered
}

// Mirror the drawer's tool-input prettification (diff / shell / JSON),
// honoring the same prettyDiff/prettyJson toggles.
function formatToolInput(
  tool: string,
  input: unknown,
  opts: ExportOpts,
): string {
  const obj = input as Record<string, unknown> | null;
  if (
    opts.prettyDiff &&
    obj &&
    typeof obj === "object" &&
    "old_string" in obj &&
    "new_string" in obj
  ) {
    const minus = String(obj.old_string ?? "")
      .split("\n")
      .map((l) => `- ${l}`)
      .join("\n");
    const plus = String(obj.new_string ?? "")
      .split("\n")
      .map((l) => `+ ${l}`)
      .join("\n");
    return highlightBlock(
      `${obj.file_path ?? ""}\n${minus}\n${plus}`.trim(),
      "",
    );
  }
  if (
    opts.prettyJson &&
    obj &&
    typeof obj === "object" &&
    typeof obj.command === "string"
  ) {
    const desc =
      typeof obj.description === "string" && obj.description.trim()
        ? `# ${obj.description.trim()}\n`
        : "";
    return highlightBlock(`${desc}${obj.command}`, "sh");
  }
  if (!opts.prettyJson) return highlightBlock(JSON.stringify(input), "json");
  return highlightBlock(
    prettyJson(input).replace(/\\n/g, "\n").replace(/\\t/g, "\t"),
    "json",
  );
}

// AskUserQuestion inputs render as a readable Q/options card, not raw JSON.
function formatAsk(input: unknown): string | null {
  const qs = (input as { questions?: unknown })?.questions;
  if (!Array.isArray(qs) || qs.length === 0) return null;
  const parts: string[] = [];
  for (const q of qs as {
    question?: string;
    options?: { label?: string; description?: string }[];
  }[]) {
    if (typeof q?.question !== "string") continue;
    const opts = (q.options ?? [])
      .map(
        (o) =>
          `<li><strong>${escapeHtml(String(o.label ?? ""))}</strong>${o.description ? ` — ${escapeHtml(o.description)}` : ""}</li>`,
      )
      .join("");
    parts.push(
      `<p class="ask-q">${escapeHtml(q.question)}</p><ul class="ask-opts">${opts}</ul>`,
    );
  }
  return parts.length ? parts.join("") : null;
}

// Render markdown honoring the export's table formatting toggle.
const md = (s: string, opts: ExportOpts) =>
  renderMarkdown(s, { tables: opts.prettyTables });

function toBlock(
  e: AgentEvent,
  opts: ExportOpts,
  seen: Set<string>,
): Block | null {
  switch (e.type) {
    case "text": {
      if (!e.content.trim()) return null;
      if (e.kind === "thinking" || e.kind === "redacted_thinking") {
        const cat = e.kind === "thinking" ? "thinking" : "redacted";
        if (!visible(opts, cat)) return null;
        return { role: "thinking", ts: Number(e.ts), html: md(e.content, opts) };
      }
      if (e.kind === "system_marker") {
        if (!visible(opts, "marker")) return null;
        return { role: "marker", ts: Number(e.ts), html: md(e.content, opts) };
      }
      if (e.content.startsWith(USER_PREFIX)) {
        const content = e.content.slice(USER_PREFIX.length).trimStart();
        const peer = parsePeerMessage(content);
        if (peer) {
          if (!visible(opts, "peer")) return null;
          return {
            role: "peer",
            ts: Number(e.ts),
            label: peer.from ?? undefined,
            html: md(peer.body, opts),
          };
        }
        const role = pollRole(content, e.meta || looksMeta(content), seen);
        if (!visible(opts, role)) return null;
        return { role, ts: Number(e.ts), html: md(content, opts) };
      }
      if (!visible(opts, e.kind === "attachment" ? "attachment" : "assistant"))
        return null;
      return { role: "assistant", ts: Number(e.ts), html: md(e.content, opts) };
    }
    case "reply":
      if (!e.content.trim()) return null;
      {
        const role = pollRole(e.content, false, seen);
        if (!visible(opts, role)) return null;
        return { role, ts: Number(e.ts), html: md(e.content, opts) };
      }
    case "tool_call": {
      if (e.tool === "AskUserQuestion") {
        const ask = formatAsk(e.input);
        if (ask)
          return {
            role: "ask",
            ts: Number(e.ts),
            label: "AskUserQuestion",
            html: ask,
          };
      }
      const isMcp = e.tool.startsWith("mcp__");
      const cat =
        e.kind === "server_tool_use" ? "server_tool" : isMcp ? "mcp" : "tool";
      if (!visible(opts, cat)) return null;
      return {
        role: "tool",
        ts: Number(e.ts),
        label: e.tool,
        html: `<pre><code>${formatToolInput(e.tool, e.input, opts)}</code></pre>`,
      };
    }
    case "tool_result":
      if (!visible(opts, resultCategory(e))) return null;
      return {
        role: "result",
        ts: Number(e.ts),
        label: e.tool,
        html: `<pre><code>${highlightBlock(e.output_summary, "")}</code></pre>`,
      };
    case "context_reset":
      if (!visible(opts, "reset")) return null;
      return {
        role: "reset",
        ts: Number(e.ts),
        html: "⟳ context reset · /clear or /compact",
      };
    case "compact_summary":
      if (!visible(opts, "compact")) return null;
      if (!e.content.trim()) return null;
      return { role: "compact", ts: Number(e.ts), html: md(e.content, opts) };
    case "turn_summary": {
      if (!visible(opts, "summary")) return null;
      const detail = e.detail.trim() || (e.status_category ?? "").trim();
      if (!detail) return null;
      return {
        role: "summary",
        ts: Number(e.ts),
        label: e.needs_action ? "needs action" : undefined,
        html: escapeHtml(detail),
      };
    }
    default:
      return null; // heartbeat, turn_end
  }
}

const ROLE_LABEL: Record<Block["role"], string> = {
  assistant: "Assistant",
  thinking: "Thinking",
  user: "User",
  poll: "Poll",
  peer: "Peer",
  system: "System",
  marker: "Marker",
  tool: "Tool",
  result: "Result",
  summary: "Summary",
  ask: "Question",
  reset: "",
  compact: "Compacted context",
};

function fmtTs(ts: number): string {
  const d = new Date(ts);
  return isNaN(d.getTime()) ? "" : d.toLocaleString(getLocale());
}

// ── Theme capture ────────────────────────────────────────────────────────────
// The app's themes (dark/light/sepia) live as `--c-*` / `--md-*` / `--syn-*`
// custom properties on <html> (variables.css, switched by [data-theme]). Read
// the ACTIVE theme's computed values at export time and bake them into the
// file's :root so the export matches what's on screen. `var()` references are
// substituted in computed values; a remaining `color-mix(...)` expression is
// still valid CSS in the standalone file. Fallbacks = the dark palette, for
// safety if a token is ever missing.
const TOKEN_FALLBACKS: Record<string, string> = {
  "--c-bg": "#0f1115",
  "--c-bg-elev": "#171a21",
  "--c-border": "#2c323d",
  "--c-text": "#e6e9ef",
  "--c-text-muted": "#9aa3b2",
  "--c-text-faint": "#6b7384",
  "--c-blue": "#5aa9ff",
  "--c-green": "#5ad6a0",
  "--c-amber": "#f0b454",
  "--c-red": "#f0716b",
  "--c-violet": "#b48ef0",
  "--role-user": "#5ad6a0",
  "--role-assistant": "#5aa9ff",
  "--role-system": "#b48ef0",
  "--role-peer": "#cc7fb0",
  "--role-poll": "#f09a5d",
  "--role-tool": "#f0b454",
  "--role-mcp": "#4fd6cf",
  "--role-thinking": "#d69d76",
  "--role-summary": "#9aa3b2",
  "--font-sans":
    "ui-sans-serif,system-ui,-apple-system,'Segoe UI',Roboto,'Helvetica Neue',Arial,sans-serif",
  "--font-mono":
    "ui-monospace,'SF Mono','JetBrains Mono','Fira Code',Menlo,Consolas,monospace",
  "--md-text": "#9aa3b2",
  "--md-strong": "#e6e9ef",
  "--md-code": "#5aa9ff",
  "--md-code-bg": "rgba(90,169,255,.12)",
  "--md-heading": "#e6e9ef",
  "--syn-keyword": "#b48ef0",
  "--syn-string": "#5ad6a0",
  "--syn-number": "#f0b454",
  "--syn-comment": "#6b7384",
  "--syn-function": "#5aa9ff",
};

function themeVarsCss(): string {
  const cs =
    typeof document !== "undefined"
      ? getComputedStyle(document.documentElement)
      : null;
  return Object.entries(TOKEN_FALLBACKS)
    .map(
      ([name, fb]) =>
        `${name}:${(cs?.getPropertyValue(name) || "").trim() || fb}`,
    )
    .join(";");
}

// Layout/structure CSS. Colors come exclusively from the captured theme tokens
// above. Print keeps the same palette (the export matches the screen theme —
// a sepia screen prints sepia); it only adds page setup + break rules.
const CSS = `
*{box-sizing:border-box}
body{margin:0;background:var(--c-bg);color:var(--c-text);font:14px/1.5 var(--font-sans);print-color-adjust:exact;-webkit-print-color-adjust:exact}
.page{max-width:900px;margin:0 auto;padding:24px 20px 48px}
header{border-bottom:1px solid var(--c-border);padding-bottom:14px;margin-bottom:18px}
header h1{font-size:18px;margin:0 0 8px;word-break:break-word}
.meta{display:flex;flex-wrap:wrap;gap:6px 14px;color:var(--c-text-muted);font-size:12px}
.meta b{color:var(--c-text);font-weight:600}
.msg{margin:10px 0;border:1px solid var(--c-border);border-left-width:3px;padding:6px 12px;border-radius:4px;background:var(--c-bg-elev)}
.msg .who{display:flex;gap:8px;align-items:baseline;font-size:11px;color:var(--c-text-faint);margin-bottom:4px}
.msg .who .r{font-weight:700;text-transform:uppercase;letter-spacing:.04em}
.msg .body{color:var(--md-text);word-break:break-word;overflow-wrap:anywhere;white-space:normal}
.user{border-color:color-mix(in srgb,var(--role-user) 45%,transparent);border-left-color:var(--role-user);background:color-mix(in srgb,var(--role-user) 14%,var(--c-bg-elev))}.user .who .r{color:var(--role-user)}
.assistant{border-color:color-mix(in srgb,var(--role-assistant) 28%,var(--c-border));border-left-color:var(--role-assistant);background:color-mix(in srgb,var(--role-assistant) 7%,var(--c-bg-elev))}.assistant .who .r{color:var(--role-assistant)}
.peer{border-color:color-mix(in srgb,var(--role-peer) 40%,transparent);border-left-color:var(--role-peer);background:color-mix(in srgb,var(--role-peer) 12%,var(--c-bg-elev))}.peer .who .r{color:var(--role-peer)}
.poll{border-color:color-mix(in srgb,var(--role-poll) 40%,transparent);border-left-color:var(--role-poll);background:color-mix(in srgb,var(--role-poll) 12%,var(--c-bg-elev))}.poll .who .r{color:var(--role-poll)}
.system{border-color:color-mix(in srgb,var(--role-system) 24%,var(--c-border));border-left-color:var(--role-system);background:color-mix(in srgb,var(--role-system) 7%,var(--c-bg-elev));opacity:.9}.system .who .r{color:var(--role-system)}
.tool{border-color:color-mix(in srgb,var(--role-tool) 26%,var(--c-border));border-left-color:var(--role-tool);background:color-mix(in srgb,var(--role-tool) 6%,var(--c-bg-elev))}.tool .who .r{color:var(--role-tool)}
.result{border-color:color-mix(in srgb,var(--c-amber) 26%,var(--c-border));border-left-color:var(--c-amber);background:color-mix(in srgb,var(--c-amber) 6%,var(--c-bg-elev))}.result .who .r{color:var(--c-amber)}
.thinking{border-color:color-mix(in srgb,var(--role-thinking) 30%,var(--c-border));border-left-color:var(--role-thinking);background:color-mix(in srgb,var(--role-thinking) 8%,var(--c-bg-elev))}.thinking .who .r{color:var(--role-thinking)}
.summary{border-color:color-mix(in srgb,var(--role-summary) 22%,var(--c-border));border-left-color:var(--role-summary);background:none;font-size:12px}.summary .who .r{color:var(--role-summary)}
.marker{border-color:var(--c-border);border-left-color:var(--c-text-faint);background:none;font-size:12px}.marker .who .r{color:var(--c-text-faint)}
.ask{border-left-color:var(--c-red)}.ask .who .r{color:var(--c-red)}
.compact{border-left-color:var(--c-amber)}
.reset{border-left:none;background:none;text-align:center;color:var(--c-text-faint);font-size:12px;margin:18px 0}
pre{margin:4px 0;padding:8px 10px;background:color-mix(in srgb,var(--c-text) 5%,var(--c-bg));border:1px solid var(--c-border);border-radius:4px;overflow-x:auto;white-space:pre-wrap;word-break:break-word;font-size:12px;line-height:1.45}
code{font-family:var(--font-mono)}
.md-pre{margin:6px 0}
.md-code{color:var(--md-code);background:var(--md-code-bg);padding:0 4px;border-radius:3px}
.md-h{display:block;font-weight:700;color:var(--md-heading);margin-top:6px}
.md-quote{display:block;border-left:2px solid var(--c-border);padding-left:8px;color:var(--c-text-faint)}
.md-li{display:block;padding-left:8px}
.md-meta-tag{color:var(--c-text-faint);font-style:italic}
strong{color:var(--md-strong)}
a{color:var(--c-blue)}
.syn-keyword,.hljs-keyword,.hljs-built_in,.hljs-type,.hljs-literal,.hljs-symbol,.hljs-selector-tag{color:var(--syn-keyword)}
.syn-string,.hljs-string,.hljs-char,.hljs-regexp{color:var(--syn-string)}
.syn-number,.hljs-number,.hljs-attr,.hljs-attribute,.hljs-variable,.hljs-template-variable{color:var(--syn-number)}
.syn-comment,.hljs-comment,.hljs-quote{color:var(--syn-comment);font-style:italic}
.syn-function,.hljs-title,.hljs-section,.hljs-name,.hljs-meta,.hljs-property{color:var(--syn-function)}
.hljs-addition{color:var(--syn-string)}.hljs-deletion{color:var(--c-red)}
.ask-q{margin:2px 0;color:var(--c-text);font-weight:600}
.ask-opts{margin:4px 0 2px;padding-left:18px}
footer{margin-top:28px;color:var(--c-text-faint);font-size:11px;text-align:center}
@media print{
body{font-size:11px}
.msg{break-inside:avoid-page}
a{text-decoration:none}
@page{margin:14mm}}
`;

export function buildConversationHtml(
  session: SessionListItem,
  events: AgentEvent[],
  opts: ExportOpts,
): string {
  const seen = new Set<string>();
  const blocks = events
    .map((e) => toBlock(e, opts, seen))
    .filter((b): b is Block => b !== null);
  const title = session.name || session.working_dir || session.id;
  const first = blocks[0]?.ts;
  const last = blocks[blocks.length - 1]?.ts;
  const meta: string[] = [
    `<span><b>session</b> ${escapeHtml(session.id)}</span>`,
    session.machine_name
      ? `<span><b>machine</b> ${escapeHtml(session.machine_name)}</span>`
      : "",
    session.model
      ? `<span><b>model</b> ${escapeHtml(session.model)}${session.effort ? ` · ${escapeHtml(session.effort)}` : ""}</span>`
      : "",
    session.working_dir
      ? `<span><b>cwd</b> ${escapeHtml(session.working_dir)}</span>`
      : "",
    first ? `<span><b>from</b> ${escapeHtml(fmtTs(first))}</span>` : "",
    last ? `<span><b>to</b> ${escapeHtml(fmtTs(last))}</span>` : "",
    `<span><b>events</b> ${blocks.length}</span>`,
  ].filter(Boolean);

  const body = blocks
    .map((b) => {
      if (b.role === "reset") return `<div class="reset">${b.html}</div>`;
      const label = b.label
        ? `${ROLE_LABEL[b.role]} · ${escapeHtml(b.label)}`
        : ROLE_LABEL[b.role];
      return `<div class="msg ${b.role}"><div class="who"><span class="r">${label}</span><span>${escapeHtml(fmtTs(b.ts))}</span></div><div class="body">${b.html}</div></div>`;
    })
    .join("\n");

  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>${escapeHtml(title)} — cctui transcript</title>
<style>:root{${themeVarsCss()}}${CSS}</style>
</head>
<body>
<div class="page">
<header><h1>${escapeHtml(title)}</h1><div class="meta">${meta.join("")}</div></header>
${body}
<footer>Exported from cctui · ${escapeHtml(new Date().toLocaleString(getLocale()))} · use your browser's Print → Save as PDF for a PDF copy</footer>
</div>
</body>
</html>
`;
}

// ── Copy-as-Markdown ────────────────────────────────────────
// Serialize the conversation to a plain-Markdown string for the clipboard, so a
// whole chat can be pasted into a PR/issue/notes. Honors the same view filters
// as the HTML export. Tool inputs / results go in fenced code blocks; the
// pretty-diff/json toggles shape the body the same way the screen does.

function fenced(body: string, lang = ""): string {
  // Avoid breaking out of the fence if the body itself contains ```.
  const safe = body.replace(/```/g, "` ` `");
  return `\`\`\`${lang}\n${safe}\n\`\`\``;
}

function toMarkdownBlock(
  e: AgentEvent,
  opts: ExportOpts,
  seen: Set<string>,
): string | null {
  const obj = (input: unknown) => input as Record<string, unknown> | null;
  switch (e.type) {
    case "text": {
      if (!e.content.trim()) return null;
      if (e.kind === "thinking" || e.kind === "redacted_thinking") {
        const cat = e.kind === "thinking" ? "thinking" : "redacted";
        if (!visible(opts, cat)) return null;
        return `**Thinking:**\n\n${e.content}`;
      }
      if (e.kind === "system_marker") {
        if (!visible(opts, "marker")) return null;
        return `_${e.content}_`;
      }
      if (e.content.startsWith(USER_PREFIX)) {
        const content = e.content.slice(USER_PREFIX.length).trimStart();
        const peer = parsePeerMessage(content);
        if (peer) {
          if (!visible(opts, "peer")) return null;
          const who = peer.from ? `Peer · ${peer.from}` : "Peer";
          return `**${who}:**\n\n${peer.body}`;
        }
        const role = pollRole(content, e.meta || looksMeta(content), seen);
        if (!visible(opts, role)) return null;
        const who =
          role === "poll" ? "Poll" : role === "system" ? "System" : "User";
        return `**${who}:**\n\n${content}`;
      }
      if (!visible(opts, e.kind === "attachment" ? "attachment" : "assistant"))
        return null;
      return `**Assistant:**\n\n${e.content}`;
    }
    case "reply":
      if (!e.content.trim()) return null;
      {
        const role = pollRole(e.content, false, seen);
        if (!visible(opts, role)) return null;
        return `**${role === "poll" ? "Poll" : "User"}:**\n\n${e.content}`;
      }
    case "tool_call": {
      const isMcp = e.tool.startsWith("mcp__");
      const cat =
        e.kind === "server_tool_use" ? "server_tool" : isMcp ? "mcp" : "tool";
      if (!visible(opts, cat)) return null;
      const o = obj(e.input);
      if (opts.prettyDiff && o && "old_string" in o && "new_string" in o) {
        const minus = String(o.old_string ?? "")
          .split("\n")
          .map((l) => `- ${l}`)
          .join("\n");
        const plus = String(o.new_string ?? "")
          .split("\n")
          .map((l) => `+ ${l}`)
          .join("\n");
        return `**Tool · ${e.tool}**\n\n${fenced(`${o.file_path ?? ""}\n${minus}\n${plus}`.trim(), "diff")}`;
      }
      if (o && typeof o.command === "string") {
        const desc =
          typeof o.description === "string" && o.description.trim()
            ? `# ${o.description.trim()}\n`
            : "";
        return `**Tool · ${e.tool}**\n\n${fenced(`${desc}${o.command}`, "sh")}`;
      }
      const json = prettyJson(e.input)
        .replace(/\\n/g, "\n")
        .replace(/\\t/g, "\t");
      return `**Tool · ${e.tool}**\n\n${fenced(json, "json")}`;
    }
    case "tool_result":
      if (!visible(opts, resultCategory(e))) return null;
      return `**Result · ${e.tool}**\n\n${fenced(e.output_summary)}`;
    case "context_reset":
      if (!visible(opts, "reset")) return null;
      return `---\n\n_⟳ context reset · /clear or /compact_\n\n---`;
    case "compact_summary":
      if (!visible(opts, "compact")) return null;
      if (!e.content.trim()) return null;
      return `**Compacted context:**\n\n${e.content}`;
    case "turn_summary": {
      if (!visible(opts, "summary")) return null;
      const detail = e.detail.trim() || (e.status_category ?? "").trim();
      if (!detail) return null;
      return `_${e.needs_action ? "Needs action" : "Summary"}: ${detail}_`;
    }
    default:
      return null;
  }
}

export function conversationToMarkdown(
  session: SessionListItem,
  events: AgentEvent[],
  opts: ExportOpts,
): string {
  const title = session.name || session.working_dir || session.id;
  const head: string[] = [`# ${title}`, ""];
  const metaBits = [
    `session: \`${session.id}\``,
    session.machine_name ? `machine: ${session.machine_name}` : "",
    session.model
      ? `model: ${session.model}${session.effort ? ` · ${session.effort}` : ""}`
      : "",
    session.working_dir ? `cwd: \`${session.working_dir}\`` : "",
  ].filter(Boolean);
  if (metaBits.length) head.push(metaBits.join(" · "), "");
  const mdSeen = new Set<string>();
  const body = events
    .map((e) => toMarkdownBlock(e, opts, mdSeen))
    .filter((b): b is string => b !== null)
    .join("\n\n");
  return `${head.join("\n")}${body}\n`;
}

/** Trigger a client-side download of the built HTML transcript. */
export function downloadConversationHtml(
  session: SessionListItem,
  events: AgentEvent[],
  opts: ExportOpts,
) {
  const html = buildConversationHtml(session, events, opts);
  const stamp = new Date().toISOString().slice(0, 10);
  const base =
    (session.name || session.id).replace(/[^\w.-]+/g, "_").slice(0, 60) ||
    "conversation";
  const blob = new Blob([html], { type: "text/html;charset=utf-8" });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = `cctui-${base}-${stamp}.html`;
  document.body.appendChild(a);
  a.click();
  a.remove();
  URL.revokeObjectURL(url);
}
