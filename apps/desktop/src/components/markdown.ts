// Minimal, safe Markdown renderer for model output and source excerpts.
//
// Security model (plan D1 constraint): raw HTML in Markdown is disabled —
// the entire source is HTML-escaped before any structural parsing, so
// untrusted content can inject no markup, no event handlers and no script.
// Links are not anchors: the URL is displayed as mono text and never becomes
// a navigable href (the desktop opens external links through an explicit
// native safe opener, not arbitrary schemes; that command is not part of the
// current capability surface, so nothing here navigates at all).
import { escapeHtml, h } from "./dom";

/** Renders a Markdown string into an `.md` element. */
export function renderMarkdown(root: HTMLElement, source: string): void {
  root.replaceChildren(...renderBlocks(source));
}

function renderBlocks(source: string): HTMLElement[] {
  const lines = source.split("\n");
  const out: HTMLElement[] = [];
  let i = 0;

  while (i < lines.length) {
    const line = lines[i];

    if (line.startsWith("```")) {
      const fence: string[] = [];
      i += 1;
      while (i < lines.length && !lines[i].startsWith("```")) fence.push(lines[i++]);
      i += 1; // consume closing fence
      out.push(h("pre", {}, h("code", { class: "mono" }, fence.join("\n"))));
      continue;
    }

    const heading = /^(#{1,4})\s+(.*)$/.exec(line);
    if (heading) {
      const level = heading[1].length;
      out.push(h(`h${level}`, { class: "md-heading" }, inlineSpan(escapeHtml(heading[2])));
      i += 1;
      continue;
    }

    if (/^\s*>\s?/.test(line)) {
      const quote: string[] = [];
      while (i < lines.length && /^\s*>\s?/.test(lines[i])) quote.push(lines[i++].replace(/^\s*>\s?/, ""));
      out.push(h("blockquote", {}, ...renderBlocks(quote.join("\n"))));
      continue;
    }

    if (/^\s*[-*]\s+/.test(line)) {
      const items: string[] = [];
      while (i < lines.length && /^\s*[-*]\s+/.test(lines[i])) items.push(lines[i++].replace(/^\s*[-*]\s+/, ""));
      const list = h("ul");
      for (const item of items) list.append(h("li", {}, inlineSpan(escapeHtml(item))));
      out.push(list);
      continue;
    }

    if (/^\s*\d+[.)]\s+/.test(line)) {
      const items: string[] = [];
      while (i < lines.length && /^\s*\d+[.)]\s+/.test(lines[i])) items.push(lines[i++].replace(/^\s*\d+[.)]\s+/, ""));
      const list = h("ol");
      for (const item of items) list.append(h("li", {}, inlineSpan(escapeHtml(item))));
      out.push(list);
      continue;
    }

    if (line.trim() === "") {
      i += 1;
      continue;
    }

    // Paragraph: consume until a blank line or a structural line.
    const para: string[] = [];
    while (i < lines.length && lines[i].trim() !== "" && !/^(#{1,4}\s|```|\s*>\s?|\s*[-*]\s+|\s*\d+[.)]\s+)/.test(lines[i])) {
      para.push(lines[i++]);
    }
    out.push(h("p", {}, inlineSpan(escapeHtml(para.join(" ")))));
  }

  return out;
}

/** Inline emphasis/code inside an already-escaped string. Returns a text
 * node or a span with the safe inline structure. */
function inlineSpan(escaped: string): Node {
  const span = h("span");
  const codeSplit = /(`[^`]+`)/g;
  const chunks = escaped.split(codeSplit);
  for (const chunk of chunks) {
    if (chunk.startsWith("`") && chunk.endsWith("`") && chunk.length > 2) {
      span.append(h("code", { class: "inline" }, chunk.slice(1, -1)));
    } else {
      const boldSplit = chunk.split(/(\*\*[^*]+\*\*)/g);
      for (const piece of boldSplit) {
        if (piece.startsWith("**") && piece.endsWith("**") && piece.length > 4) {
          span.append(h("strong", {}, piece.slice(2, -2)));
        } else {
          span.append(document.createTextNode(piece));
        }
      }
    }
  }
  return span;
}
