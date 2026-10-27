/**
 * Protocol 2 exact runtime result validator for the Ironbark core contract.
 *
 * Everything here compiles the LOCAL generated schema (packages/contracts/
 * schema.json, emitted by `cargo run -p ironbark-core --bin export_contracts`
 * from the Rust declarations). There is no independent handwritten command
 * schema and no remote resolution: `$ref`s are asserted to be local (`#/...`)
 * before compilation, and ajv cannot fetch anything, so a schema that escaped
 * the generation boundary fails closed.
 *
 * Custom format hooks make the decimal-string wire fields exact on the TS side
 * exactly as the Rust `wire` serde modules are:
 *   - `wire-u64` / `wire-i64` — canonical decimal lexemes with the full
 *     native range (parseWireU64 / parseWireI64); the `pattern` alone cannot
 *     express the numeric range.
 *   - `uint` / `uint8` / `uint16` / `uint32` — the remaining numeric bounded
 *     integer formats schemars emits (ajv-formats only ships int32/int64).
 *
 * `validateResult(command, value)` is the single unwrap path for `CoreReply::
 * Ok`: it validates the whole `{command, value}` envelope against the
 * CommandResult schema and then requires the envelope's command tag to equal
 * the requested command. A mismatched, echoed, unknown-field or out-of-range
 * result cannot pass, so `ResultOf<C>` is only ever produced for the matching
 * leaf.
 */

import Ajv2020, { type ValidateFunction, type ErrorObject } from 'ajv/dist/2020';
import addFormats from 'ajv-formats';
import { parseWireI64, parseWireU64 } from './wire.ts';
import type { CommandResult, CoreCommand, ResultOf } from './generated.ts';

/** The generated schema is a build artifact, read once at import time. */
const schema = JSON.parse(await Bun.file(new URL('../schema.json', import.meta.url)).text()) as unknown;

/** Fail closed on any `$ref` that would resolve outside the local schema. */
function assertLocalOnlyRefs(node: unknown, path = '$'): void {
  if (Array.isArray(node)) {
    for (let i = 0; i < node.length; i++) assertLocalOnlyRefs(node[i], `${path}/${i}`);
    return;
  }
  if (node !== null && typeof node === 'object') {
    for (const [key, value] of Object.entries(node as Record<string, unknown>)) {
      if (key === '$ref' && (typeof value !== 'string' || !value.startsWith('#'))) {
        throw new TypeError(`validate.ts: ${path}.$ref resolves outside the local schema (${String(value)}); remote refs are disabled`);
      }
      assertLocalOnlyRefs(value, `${path}/${key}`);
    }
  }
}

let validateCommandResultFn: ValidateFunction<CommandResult> | null = null;

function commandResultValidator(): ValidateFunction<CommandResult> {
  if (validateCommandResultFn !== null) return validateCommandResultFn;
  assertLocalOnlyRefs(schema);

  const ajv = new Ajv2020({ strict: true, strictSchema: false, validateFormats: true });
  // uuid/double/…. ajv-formats does not ship uint8/uint16/uint32/uint (usize)
  // or the decimal wire formats, so those are registered here.
  addFormats(ajv);
  ajv.addFormat('wire-u64', { type: 'string', validate: (value: unknown) => { try { parseWireU64(value); return true; } catch { return false; } } });
  ajv.addFormat('wire-i64', { type: 'string', validate: (value: unknown) => { try { parseWireI64(value); return true; } catch { return false; } } });
  for (const [format, max] of [['uint', Number.MAX_SAFE_INTEGER], ['uint8', 255], ['uint16', 65535], ['uint32', 4_294_967_295]] as const) {
    ajv.addFormat(format, { type: 'number', validate: (value: unknown) => typeof value === 'number' && Number.isSafeInteger(value) && value >= 0 && value <= max });
  }
  // Defensive: schemars may emit int64/uint64 numerically for fields not yet
  // converted; keep them exact-or-loud rather than silently permissive.
  ajv.addFormat('uint64', { type: 'number', validate: (value: unknown) => false });
  ajv.addFormat('int64', { type: 'number', validate: (value: unknown) => false });

  ajv.addSchema(schema as Parameters<typeof ajv.addSchema>[0], 'ironbark');
  // wrap the CodeStyle: compile the CommandResult definition for validation.
  validateCommandResultFn = ajv.compile<CommandResult>({ $ref: 'ironbark#/$defs/CommandResult' });
  return validateCommandResultFn;
}

function describeErrors(errors: ErrorObject[] | null | undefined): string {
  if (!errors) return 'no detail';
  return errors.slice(0, 3).map(e => `${e.instancePath || '/'} ${e.message ?? 'invalid'}${e.params && Object.keys(e.params).length ? ` (${JSON.stringify(e.params)})` : ''}`).join('; ');
}

/**
 * Validate one `CoreReply::Ok` value (a command-tagged `CommandResult`
 * envelope) against the locally generated CommandResult schema. Throws a
 * `TypeError` on any shape, tag, unknown-field, format or range violation.
 */
export function validateCommandResult(value: unknown): CommandResult {
  if (typeof value !== 'object' || value === null) {
    throw new TypeError(`validate.ts: result is not an object envelope: ${String(value)}`);
  }
  const validator = commandResultValidator();
  if (!validator(value)) {
    throw new TypeError(`validate.ts: result failed canonical CommandResult schema: ${describeErrors(validator.errors)}`);
  }
  return value as CommandResult;
}

/**
 * Unwrap the leaf of one command's reply. Validates the full envelope against
 * the generated schema and rejects any envelope whose `command` tag does not
 * equal the requested command — a mismatched or echoed result cannot produce
 * `ResultOf<C>`. The single `as ResultOf<C>` here is the validated boundary:
 * TS cannot narrow a generic discriminant, but the schema check above is the
 * runtime proof that `value` is exactly this command's leaf.
 */
export function validateResult<C extends CoreCommand>(command: C, value: unknown): ResultOf<C> {
  const envelope = validateCommandResult(value);
  if (envelope.command !== command.type) {
    throw new TypeError(`validate.ts: reply envelope carries command "${envelope.command}" but "${command.type}" was requested; mismatched or echoed result rejected`);
  }
  return envelope.value as ResultOf<C>;
}