// Protocol 2 exact wire contract: canonical decimal-string 64-bit integers.
//
// JSON consumers (JavaScript) cannot represent Rust u64/i64 exactly as
// JSON numbers, so every 64-bit integer crosses the language boundary as
// a canonical decimal string. This module provides the serde helpers and
// JSON-schema markers consumed by contracts, policy, store, and
// conversation records.

use std::borrow::Cow;

/// Canonical unsigned wire lexeme: ASCII digits with no
/// leading zero (the lone zero is the exception). `parse`
/// alone accepts "00", "01" and "+1", so the canonical
/// form is enforced before the numeric parse.
fn ensure_canonical_unsigned(text: &str) -> Result<(), String> {
    if text.is_empty()
        || !text.bytes().all(|byte| byte.is_ascii_digit())
        || (text.len() > 1 && text.starts_with('0'))
    {
        return Err(format!("non-canonical unsigned wire integer: {text:?}"));
    }
    Ok(())
}

/// Canonical signed wire lexeme: an optional minus sign
/// followed by canonical unsigned digits; a signed zero
/// ("-0") is rejected as non-canonical.
fn ensure_canonical_signed(text: &str) -> Result<(), String> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    if text.is_empty()
        || text.starts_with('+')
        || digits.is_empty()
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
        || (digits.len() > 1 && digits.starts_with('0'))
        || (text.starts_with('-') && digits == "0")
    {
        return Err(format!("non-canonical signed wire integer: {text:?}"));
    }
    Ok(())
}

pub mod u64 {
    use serde::{Deserialize, Deserializer, Serializer};
    use super::ensure_canonical_unsigned;

    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        let text = String::deserialize(deserializer)?;
        ensure_canonical_unsigned(&text).map_err(serde::de::Error::custom)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

pub mod i64 {
    use serde::{Deserialize, Deserializer, Serializer};
    use super::ensure_canonical_signed;

    pub fn serialize<S: Serializer>(value: &i64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
        let text = String::deserialize(deserializer)?;
        ensure_canonical_signed(&text).map_err(serde::de::Error::custom)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

pub mod option_u64 {
    use serde::{Deserialize, Deserializer, Serializer};
    use super::ensure_canonical_unsigned;

    pub fn serialize<S: Serializer>(value: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(number) => serializer.serialize_some(&number.to_string()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u64>, D::Error> {
        let text: Option<String> = Option::deserialize(deserializer)?;
        match text {
            Some(number) => {
                ensure_canonical_unsigned(&number).map_err(serde::de::Error::custom)?;
                number.parse().map(Some).map_err(serde::de::Error::custom)
            }
            None => Ok(None),
        }
    }
}

pub mod option_i64 {
    use serde::{Deserialize, Deserializer, Serializer};
    use super::ensure_canonical_signed;

    pub fn serialize<S: Serializer>(value: &Option<i64>, serializer: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(number) => serializer.serialize_some(&number.to_string()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<i64>, D::Error> {
        let text: Option<String> = Option::deserialize(deserializer)?;
        match text {
            Some(number) => {
                ensure_canonical_signed(&number).map_err(serde::de::Error::custom)?;
                number.parse().map(Some).map_err(serde::de::Error::custom)
            }
            None => Ok(None),
        }
    }
}

/// One canonical decimal wire string schema: the named
/// format hook (which checks the full native range) plus the
/// pattern that already rejects non-canonical lexemes.
fn wire_string_schema(description: &str, format: &str, pattern: &str) -> schemars::Schema {
    let mut object = serde_json::Map::new();
    object.insert("description".to_string(), serde_json::Value::String(description.to_string()));
    object.insert("format".to_string(), serde_json::Value::String(format.to_string()));
    object.insert("pattern".to_string(), serde_json::Value::String(pattern.to_string()));
    object.insert("type".to_string(), serde_json::Value::String("string".to_string()));
    schemars::Schema::from(object)
}

/// JSON-schema marker for a string-encoded u64 field.
pub struct WireU64Schema;

impl schemars::JsonSchema for WireU64Schema {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("WireU64Schema")
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        wire_string_schema(
            "canonical unsigned 64-bit decimal string (0..=18446744073709551615)",
            "wire-u64",
            "^(0|[1-9][0-9]*)$",
        )
    }
}

/// JSON-schema marker for a string-encoded i64 field.
pub struct WireI64Schema;

impl schemars::JsonSchema for WireI64Schema {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("WireI64Schema")
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        wire_string_schema(
            "canonical signed 64-bit decimal string (-9223372036854775808..=9223372036854775807)",
            "wire-i64",
            "^(0|-?[1-9][0-9]*)$",
        )
    }
}
