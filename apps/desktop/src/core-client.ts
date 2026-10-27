// Typed transport for the durable Ironbark core. All wire shapes mirror
// crates/ironbark-core/src/contracts.rs (serde tag = "type", snake_case).
// The webview only ever sees typed values: the socket nonce, session handling
// and request correlation live in the native layer.
//
// D2/D3 extend the command union with the P2 conversation-tree commands and
// the P3/P4/P5, connection, harness and recovery commands. Commands whose
// dispatch arm is not yet wired in the core surface the core's typed error
// (e.g. `invalid_command`) to the UI — the UI never fakes success.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  Citation,
  ConnectionConfig,
  ContentPart,
  ContextPackage,
  MemoryDecision,
  PermissionDecision,
  RecoveryDecision,
  RoutePin,
  SourceInput,
} from "@ironbark/contracts";

/** Webview-safe UUID source (browser WebCrypto; no Node in the webview). */
const uuid = (): string => crypto.randomUUID();

export interface Project {
  id: string;
  name: string;
  root: string;
  created_at: number;
}

/** Status {} reply value. */
export interface StatusReply {
  protocol: number;
  projects: number;
  last_seq: number;
  abandoned_requests: number;
}

/** Doctor {} reply value. */
export interface DoctorReply {
  protocol: number;
  sqlite_integrity: string;
  foreign_keys: boolean;
  schema_version: number;
  journal_mode: string;
}

export type CoreEventPayload =
  | { type: "state_changed"; entity_id: string; state: string }
  | { type: "artifact_added"; artifact_id: string; digest: string }
  | { type: "session_bound"; session: { kind: string; id: string } }
  | { type: string; [key: string]: unknown };

export interface EventEnvelope {
  protocol: number;
  seq: number;
  request_id?: string;
  project_id?: string;
  run_id?: string;
  timestamp: number;
  payload: CoreEventPayload;
}

/** The desktop's wire command surface. The P2 conversation commands match the
 * core's `CoreCommand` serde tags exactly. The D3 commands (knowledge_*,
 * memory_*, citations_*, retrieval, connections_*, harness_*, judgment_*,
 * approval_*, run_*) are wired by the core integration owner; until an arm
 * exists the core answers with a typed `invalid_command`/dependency error,
 * which the UI displays as the real unsupported state. */
export type CoreCommand =
  | { type: "status" }
  | { type: "events"; after: number }
  | { type: "project_list" }
  | { type: "project_create"; name: string; root: string }
  | { type: "doctor" }
  | { type: "shutdown" }
  // P2 conversation tree
  | { type: "conversation_create"; project_id: string }
  | { type: "conversation_history"; conversation_id: string }
  | { type: "conversation_rename"; conversation_id: string; title: string }
  | { type: "conversation_search"; project_id: string; query: string; include_archived: boolean; limit: number }
  | { type: "conversation_export"; conversation_id: string; format: "json" | "markdown" }
  | { type: "conversation_edit"; message_id: string; parts: ContentPart[] }
  | {
      type: "conversation_queue_turn";
      conversation_id: string;
      branch_id: string;
      parts: ContentPart[];
      pin: RoutePin;
      context?: ContextPackage;
    }
  | { type: "conversation_regenerate"; message_id: string; pin: RoutePin }
  | { type: "conversation_archive"; conversation_id: string }
  | { type: "conversation_delete"; conversation_id: string; resolution: { run_ids: string[] } }
  | { type: "run_interrupt"; run_id: string }
  // P3 sources + citations
  | { type: "sources_list"; project_id: string }
  | { type: "source_ingest"; project_id: string; source: SourceInput }
  | { type: "source_revisions"; source_id: string }
  | { type: "citation_resolve"; project_id: string; citation: Citation }
  | { type: "source_delete"; project_id: string; source_id: string }
  // P4 retrieval
  | { type: "retrieval_search"; project_id: string; query: string; limit: number }
  | { type: "index_status"; project_id: string }
  // P5 memory
  | { type: "memory_list"; project_id: string }
  | { type: "memory_propose"; project_id: string; text: string; provenance: string; input_refs?: string[] }
  | { type: "memory_decide"; memory_id: string; decision: MemoryDecision; text?: string }
  | { type: "memory_forget"; memory_id: string }
  // Connections / settings
  | { type: "connections_list" }
  | { type: "connection_register"; config: ConnectionConfig }
  | { type: "connection_probe"; connection_id: string }
  | { type: "connection_delete"; connection_id: string }
  | { type: "harness_descriptors" }
  // Approvals
  | { type: "approvals_pending" }
  | { type: "approval_decide"; action_id: string; decision: PermissionDecision }
  // Recovery
  | { type: "run_recovery"; run_id: string }
  | { type: "run_reconcile"; run_id: string; decision: RecoveryDecision };

export type ConnectionState = "connecting" | "live" | "error";

interface CoreReplyWire {
  type: "ok" | "error";
  value?: unknown;
  code?: string;
  message?: string;
}

/** Typed view of a core error reply: the code is the stable capability or
 * policy signal the UI branches on (e.g. `invalid_command` for a dispatch
 * arm that is not wired yet, `run_dependency_missing`, `route_denied`). */
export class CoreError extends Error {
  readonly code: string;
  constructor(code: string, message: string) {
    super(message);
    this.code = code;
    this.name = "CoreError";
  }
}

/** The desktop surface of the core IPC protocol, wrapped over the three
 * Tauri commands. `status` uses the dedicated connect command; everything
 * else dispatches through `core_request` under a caller-chosen request ID.
 * Repeating a Send reuses the same request ID, so the core de-duplicates it
 * into the original run instead of double-dispatching. */
export class CoreClient {
  async status(): Promise<StatusReply> {
    const reply = await invoke<CoreReplyWire>("connect_core");
    return this.unwrap(reply) as StatusReply;
  }

  async doctor(): Promise<DoctorReply> {
    return this.command<DoctorReply>({ type: "doctor" });
  }

  async projectList(): Promise<Project[]> {
    return this.command<Project[]>({ type: "project_list" });
  }

  async events(after: number): Promise<EventEnvelope[]> {
    return this.command<EventEnvelope[]>({ type: "events", after });
  }

  /** Dispatch one command under a caller-stable request ID. When `requestId`
   * is omitted a fresh ID is generated for this call. */
  async command<T>(command: CoreCommand, requestId?: string): Promise<T> {
    const reply = await invoke<CoreReplyWire>("core_request", {
      requestId: requestId ?? uuid(),
      command,
    });
    return this.unwrap(reply) as T;
  }

  /** Subscribe to durable events. Resolves with the unlisten; the native
   * layer reconnects from its high-water mark automatically. */
  async subscribe(after: number, onEvent: (e: EventEnvelope) => void, onState?: (state: ConnectionState) => void): Promise<UnlistenFn> {
    onState?.("connecting");
    const unlisten = await listen<EventEnvelope>("core-event", (ev) => onEvent(ev.payload));
    try {
      await invoke("subscribe_events", { afterSeq: after });
      onState?.("live");
    } catch (err) {
      onState?.("error");
      await unlisten();
      throw err;
    }
    return unlisten;
  }


  private unwrap(reply: CoreReplyWire): unknown {
    if (reply.type !== "ok") {
      throw new CoreError(reply.code ?? "unknown", reply.message ?? "core error");
    }
    return reply.value;
  }
}
