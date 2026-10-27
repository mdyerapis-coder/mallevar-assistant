//! P3 source-ingestion smoke: drives the real prebuilt core+CLI inside an
//! isolated XDG sandbox. Ingests real Markdown/HTML/text/PDF fixtures and
//! proves the honest typed failures for an encrypted PDF, an unavailable
//! extractor and non-text media. It reports exactly what the core returned and
//! never writes a fabricated "qualified" result.
import { strict as assert } from "node:assert";
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import type { Subprocess } from "bun";
import type { CoreCommand, Project, SourceRevision } from "../../packages/contracts/src/generated";

const projectFlag = process.argv.indexOf("--project");
const requestedProject = projectFlag < 0 ? undefined : process.argv[projectFlag + 1];
const binary = resolve(process.env.IRONBARK_CLI_BIN ?? "target/debug/ironbark-cli");
const coreBinary = resolve(process.env.IRONBARK_CORE_BIN ?? "target/debug/ironbark-core");
const reportDir = ".superpowers/sdd/2026-10-12-ironbark-assistant";
const report = `${reportDir}/P3-source-ingest.json`;
const evidence: Record<string, unknown> = {
  state: "running",
  mechanism: "actual prebuilt core + CLI in an isolated XDG sandbox; real extraction",
  requested_project: requestedProject ?? null,
  simulated_model_success: false,
};

/// The isolated XDG environment every spawned process must share: the CLI must
/// talk to the sandboxed daemon, never to whatever daemon the developer's own
/// environment happens to have running.
let sandbox: NodeJS.ProcessEnv | undefined;

interface CliOutcome<T> { status: number; value?: T; code?: string; message?: string }

/// The CLI prints one `CoreReply` JSON DTO on stdout; a failure carries the
/// real `CoreError` code as `code: message` on stderr with a nonzero exit.
async function call<T>(command: CoreCommand): Promise<CliOutcome<T>> {
  assert(sandbox, "the sandbox environment must be established before any CLI call");
  const child = Bun.spawn([binary, "command", "--request-id", crypto.randomUUID(), JSON.stringify(command)], { env: sandbox, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  const timer = setTimeout(() => child.kill("SIGTERM"), 60_000);
  try {
    const [stdout, stderr, status] = await Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited]);
    if (status === 0) return { status, value: JSON.parse(stdout.trim()) as T };
    const match = /^([a-z0-9_]+): ([\s\S]*)$/.exec(stderr.trim());
    return { status, code: match?.[1] ?? "untyped_error", message: match?.[2] ?? (stderr.trim() || stdout.trim()) };
  } finally { clearTimeout(timer); }
}

async function cli<T>(command: CoreCommand): Promise<T> {
  const outcome = await call<T>(command);
  if (outcome.status !== 0) throw new Error(`${command.type} failed (${outcome.code}): ${outcome.message}`);
  return outcome.value as T;
}

/** Decode byte chunks until `predicate` matches an accumulated line. */
async function readUntilReady(stream: ReadableStream<Uint8Array>, predicate: (line: string) => boolean): Promise<boolean> {
  const decoder = new TextDecoder();
  let buffer = "";
  const reader = stream.getReader();
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      if (value) buffer += decoder.decode(value, { stream: true });
      const lines = buffer.split("\n");
      buffer = lines.pop() ?? "";
      for (const line of lines) if (predicate(line.trim())) return true;
    }
  } finally { reader.releaseLock(); }
  return predicate(buffer.trim());
}

async function startCore(): Promise<Subprocess<"ignore", "pipe", "pipe">> {
  assert(sandbox, "the sandbox environment must exist before starting the core");
  const child = Bun.spawn([coreBinary], { env: sandbox, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  const timer = setTimeout(() => child.kill("SIGTERM"), 10_000);
  try {
    if (await readUntilReady(child.stdout, (line) => line.includes("CORE_READY"))) return child;
    const stderr = await new Response(child.stderr).text();
    throw new Error(`core did not report ready: ${stderr.trim()}`);
  } finally { clearTimeout(timer); }
}

// ── fixtures: real bytes written into the sandbox, never checked into the repo ──
const encoder = new TextEncoder();

/** A minimal single-page PDF with a real text object and correct xref offsets,
 *  so the host's `pdftotext` can extract it — or honestly refuse it. */
function minimalPdf(text: string): Uint8Array {
  const escaped = text.replace(/[\\()]/g, (c) => `\\${c}`);
  const content = `BT /F1 18 Tf 72 720 Td (${escaped}) Tj ET`;
  const objects = [
    "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n",
    "2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n",
    "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>\nendobj\n",
    `4 0 obj\n<< /Length ${content.length} >>\nstream\n${content}\nendstream\nendobj\n`,
    "5 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n",
  ];
  let pdf = "%PDF-1.4\n";
  const offsets: number[] = [];
  for (const object of objects) { offsets.push(pdf.length); pdf += object; }
  const xrefStart = pdf.length;
  let xref = `xref\n0 ${objects.length + 1}\n0000000000 65535 f \n`;
  for (const offset of offsets) xref += `${offset.toString().padStart(10, "0")} 00000 n \n`;
  pdf += xref;
  pdf += `trailer\n<< /Size ${objects.length + 1} /Root 1 0 R >>\nstartxref\n${xrefStart}\n%%EOF\n`;
  return encoder.encode(pdf);
}

/// `revision` is the required outcome; `codes` (when present) are the typed
/// failures that are ALSO acceptable (an installed tool may be missing).
interface Expect { revision: boolean; codes?: string[] }
interface Fixture { name: string; bytes: Uint8Array; expect: Expect; note: string }

const fixtures: Fixture[] = [
  { name: "note.md", bytes: encoder.encode("# Workshop\nfirst line of the note\n\n# Tools\ntorque wrench specification\n"), expect: { revision: true }, note: "Markdown heading/section locators" },
  { name: "page.html", bytes: encoder.encode("<!DOCTYPE html><html><head><title>x</title><script>evil()</script></head><body><h1>Workshop</h1><p>html body text</p></body></html>"), expect: { revision: true }, note: "sanitised HTML section anchors" },
  { name: "plain.txt", bytes: encoder.encode("plain text fixture\nsecond line\n"), expect: { revision: true }, note: "text/line locators" },
  { name: "report.pdf", bytes: minimalPdf("workshop report"), expect: { revision: true, codes: ["extractor_unavailable", "malformed_pdf", "pdf_extraction_failed", "extraction_empty"] }, note: "PDF via the installed pdftotext, honestly unavailable otherwise" },
  { name: "notes.docx", bytes: encoder.encode("this is not a real docx container"), expect: { revision: false, codes: ["extractor_unavailable"] }, note: "DOCX needs pandoc (unavailable => typed refusal, never blank success)" },
  { name: "sheet.xlsx", bytes: encoder.encode("this is not a real xlsx container"), expect: { revision: false, codes: ["extractor_unavailable"] }, note: "XLSX needs calamine (unavailable => typed refusal)" },
  { name: "scan.png", bytes: new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d]), expect: { revision: false, codes: ["unsupported_encoding"] }, note: "non-UTF-8 image bytes have no qualified text extractor" },
  { name: "locked.pdf", bytes: encoder.encode("%PDF-1.7\n1 0 obj\n<< /Type /Catalog >>\nendobj\ntrailer\n<< /Root 1 0 R /Encrypt 2 0 R >>\n%%EOF\n"), expect: { revision: false, codes: ["source_encrypted"] }, note: "an encrypted PDF is refused, never indexed blank" },
];

let core: Subprocess<"ignore", "pipe", "pipe"> | undefined;
let temp: string | undefined;

try {
  temp = await mkdtemp(join(tmpdir(), "ibp3-"));
  const env: NodeJS.ProcessEnv = { ...process.env, HOME: temp, XDG_DATA_HOME: join(temp, "d"), XDG_STATE_HOME: join(temp, "s"), XDG_RUNTIME_DIR: join(temp, "r") };
  for (const dir of [env.XDG_DATA_HOME!, env.XDG_STATE_HOME!, env.XDG_RUNTIME_DIR!]) await mkdir(dir, { recursive: true, mode: 0o700 });
  sandbox = env;

  core = await startCore();
  evidence.core = "ready";

  const root = join(temp, "root");
  await mkdir(root, { recursive: true });
  const projectId: string = requestedProject ?? (await cli<Project>({ type: "project_create", name: "source-ingest", root })).id;
  evidence.project_id = projectId;

  const fixtureDir = join(temp, "fixtures");
  await mkdir(fixtureDir, { recursive: true });

  const results: unknown[] = [];
  for (const fixture of fixtures) {
    const path = join(fixtureDir, fixture.name);
    await writeFile(path, fixture.bytes);
    const ingest: CoreCommand = { type: "knowledge_ingest", project_id: projectId, source: { type: "file", path }, disclosure: { type: "local_only" } };
    const outcome = await call<SourceRevision>(ingest);
    if (outcome.status === 0) {
      assert(fixture.expect.revision, `${fixture.name}: expected a typed failure, but the core published a revision`);
      const revision = outcome.value!;
      assert(revision.locators > 0, `${fixture.name}: a published revision must carry content units`);
      results.push({ fixture: fixture.name, note: fixture.note, outcome: "revision", mime: revision.mime, extractor_id: revision.extractor_id, extractor_version: revision.extractor_version, locators: revision.locators, source_id: revision.source_id, digest: revision.digest });
    } else {
      const allowed = fixture.expect.codes ?? [];
      assert(allowed.includes(outcome.code!), `${fixture.name}: unexpected failure ${outcome.code}: ${outcome.message}`);
      results.push({ fixture: fixture.name, note: fixture.note, outcome: "typed_failure", code: outcome.code, message: outcome.message });
    }
  }
  evidence.fixtures = results;

  // Idempotency: the same bytes under a fresh request identity reuse the revision.
  const notePath = join(fixtureDir, "note.md");
  const first = await cli<SourceRevision>({ type: "knowledge_ingest", project_id: projectId, source: { type: "file", path: notePath }, disclosure: { type: "local_only" } });
  const second = await cli<SourceRevision>({ type: "knowledge_ingest", project_id: projectId, source: { type: "file", path: notePath }, disclosure: { type: "local_only" } });
  assert.equal(second.source_id, first.source_id, "re-ingesting identical bytes must reuse the source");
  assert.equal(second.digest, first.digest, "re-ingesting identical bytes must reuse the digest");
  evidence.idempotent = { source_id: first.source_id, digest: first.digest, locators: first.locators };

  evidence.state = "passed";
} catch (error) {
  evidence.state = "failed";
  evidence.error = error instanceof Error ? error.message : String(error);
  process.exitCode = 1;
} finally {
  if (core) { core.kill("SIGTERM"); await core.exited.catch(() => {}); }
  if (temp) await rm(temp, { recursive: true, force: true });
  await mkdir(reportDir, { recursive: true });
  await writeFile(report, JSON.stringify(evidence, null, 2) + "\n");
  console.log(JSON.stringify(evidence));
}
