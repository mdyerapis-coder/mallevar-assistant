/**
 * Protocol 2 exact wire contract — canonical decimal-string 64-bit integers.
 *
 * Every 64-bit integer crosses JSON as a canonical decimal STRING, never as a
 * JSON number (JS numbers cannot represent the full u64/i64 range exactly).
 * `parseWireU64`/`parseWireI64` turn a canonical wire lexeme into `bigint`;
 * `formatWireU64`/`formatWireI64` produce the canonical lexeme from `bigint`.
 *
 * Canonical rules (both directions, both implementations in Rust and TS):
 *  - `WireU64`: `"0"` or a nonzero digit followed by digits, max
 *    18446744073709551615. No `+`, no leading zeros, nothing else.
 *  - `WireI64`: `"0"` or an optional leading `-` followed by a nonzero digit
 *    and digits, range -9223372036854775808..9223372036854775807. No plus
 *    sign, no leading zeros, no negative zero.
 * Numbers, booleans, null and any noncanonical/out-of-range lexeme are
 * rejected.
 */

const U64_MAX = 18446744073709551615n;
const I64_MIN = -9223372036854775808n;
const I64_MAX = 9223372036854775807n;

/** `"0"` or a nonzero ASCII decimal digit followed by ASCII decimal digits. */
const UNSIGNED_LEXEME = /^(0|[1-9][0-9]*)$/;
/** `"0"` or an optional `-` plus a nonzero ASCII digit followed by digits. */
const SIGNED_LEXEME = /^(0|-?[1-9][0-9]*)$/;

function fail(value: unknown, kind: string): never {
  const repr = typeof value === 'string' ? JSON.stringify(value) : String(value);
  throw new TypeError(
    `invalid ${kind} wire value: ${repr} is not a canonical decimal string`,
  );
}

/** Parse a canonical unsigned 64-bit decimal string into `bigint`. */
export function parseWireU64(value: unknown): bigint {
  if (typeof value !== 'string' || !UNSIGNED_LEXEME.test(value)) {
    fail(value, 'wire u64');
  }
  const parsed = BigInt(value);
  if (parsed > U64_MAX) fail(value, 'wire u64');
  return parsed;
}

/** Parse a canonical signed 64-bit decimal string into `bigint`. */
export function parseWireI64(value: unknown): bigint {
  if (typeof value !== 'string' || !SIGNED_LEXEME.test(value)) {
    fail(value, 'wire i64');
  }
  const parsed = BigInt(value);
  if (parsed < I64_MIN || parsed > I64_MAX) fail(value, 'wire i64');
  return parsed;
}

/** Format a `bigint` as a canonical unsigned 64-bit decimal string. */
export function formatWireU64(value: bigint): string {
  if (typeof value !== 'bigint' || value < 0n || value > U64_MAX) {
    fail(value, 'wire u64');
  }
  return value.toString();
}

/** Format a `bigint` as a canonical signed 64-bit decimal string. */
export function formatWireI64(value: bigint): string {
  if (typeof value !== 'bigint' || value < I64_MIN || value > I64_MAX) {
    fail(value, 'wire i64');
  }
  return value.toString();
}