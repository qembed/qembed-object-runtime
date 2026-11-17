//! RFC 8785 JSON Canonicalization Scheme validation.
//!
//! Parsing uses a duplicate-key-rejecting visitor before JCS serialization.
//! This is important because ordinary JSON object maps can silently overwrite
//! duplicate members before canonicalization sees them.

use std::collections::BTreeMap;

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Number;

/// Canonicalizes valid I-JSON input according to RFC 8785.
///
/// Duplicate object member names, invalid UTF-8, malformed JSON, and numbers
/// that cannot be represented by the JSON parser are rejected.
pub fn canonicalize_json(input: &[u8]) -> Result<Vec<u8>, JcsError> {
    let mut deserializer = serde_json::Deserializer::from_slice(input);
    let value = StrictValue::deserialize(&mut deserializer).map_err(|_| JcsError::InvalidJson)?;
    deserializer.end().map_err(|_| JcsError::InvalidJson)?;
    serde_jcs::to_vec(&value).map_err(|_| JcsError::InvalidJson)
}

/// Verifies that bytes are already the exact RFC 8785 canonical representation.
///
/// This function never rewrites the supplied bytes. Callers that need a
/// canonical representation from ordinary JSON should use canonicalize_json
/// and then persist the returned bytes.
pub fn validate_canonical_json(input: &[u8]) -> Result<(), JcsError> {
    let canonical = canonicalize_json(input)?;
    if canonical == input {
        Ok(())
    } else {
        Err(JcsError::NonCanonicalJson)
    }
}

/// Failure while parsing or validating RFC 8785 JSON.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JcsError {
    /// Input is not valid JSON/I-JSON, contains duplicate keys, or has invalid UTF-8.
    InvalidJson,
    /// Input is valid JSON but is not its RFC 8785 canonical byte representation.
    NonCanonicalJson,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum StrictValue {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<Self>),
    Object(BTreeMap<String, Self>),
}

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct StrictValueVisitor;

        impl<'de> Visitor<'de> for StrictValueVisitor {
            type Value = StrictValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a valid JSON value with unique object member names")
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(StrictValue::Null)
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(StrictValue::Null)
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                Ok(StrictValue::Bool(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(StrictValue::Number(Number::from(value)))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(StrictValue::Number(Number::from(value)))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Number::from_f64(value)
                    .map(StrictValue::Number)
                    .ok_or_else(|| E::custom("non-finite JSON number"))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StrictValue::String(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(StrictValue::String(value))
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element()? {
                    values.push(value);
                }
                Ok(StrictValue::Array(values))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut values = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, StrictValue>()? {
                    if values.insert(key, value).is_some() {
                        return Err(de::Error::custom("duplicate JSON object member"));
                    }
                }
                Ok(StrictValue::Object(values))
            }
        }

        deserializer.deserialize_any(StrictValueVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_8785_number_serialization_vector() {
        let input = br#"{"numbers":[333333333.33333329,1E30,4.50,2e-3,0.000000000000000000000000001]}"#;
        let expected = br#"{"numbers":[333333333.3333333,1e+30,4.5,0.002,1e-27]}"#;

        assert_eq!(canonicalize_json(input).expect("valid JSON"), expected);
        validate_canonical_json(expected).expect("expected bytes are canonical");
    }

    #[test]
    fn sorts_object_keys_using_utf16_order() {
        let input = "{\"😀\":1,\"€\":2}".as_bytes();
        let expected = "{\"€\":2,\"😀\":1}".as_bytes();

        assert_eq!(canonicalize_json(input).expect("valid JSON"), expected);
    }

    #[test]
    fn rejects_duplicate_object_keys_at_every_depth() {
        for input in [
            br#"{"a":1,"a":2}"#.as_slice(),
            br#"{"outer":{"x":1,"x":2}}"#.as_slice(),
        ] {
            assert_eq!(canonicalize_json(input), Err(JcsError::InvalidJson));
        }
    }

    #[test]
    fn rejects_non_finite_numbers() {
        for input in [b"NaN".as_slice(), b"Infinity".as_slice(), b"-Infinity".as_slice()] {
            assert_eq!(canonicalize_json(input), Err(JcsError::InvalidJson));
        }
    }

    #[test]
    fn rejects_invalid_utf8_and_malformed_json() {
        assert_eq!(canonicalize_json(&[0xff, 0xfe]), Err(JcsError::InvalidJson));
        assert_eq!(canonicalize_json(b"{\"a\":}"), Err(JcsError::InvalidJson));
    }

    #[test]
    fn rejects_valid_but_noncanonical_bytes() {
        for input in [
            br#"{ "a":1}"#.as_slice(),
            br#"{"b":2,"a":1}"#.as_slice(),
            br#"{"n":1.0}"#.as_slice(),
            br#"{"s":"\/"}"#.as_slice(),
        ] {
            assert_eq!(validate_canonical_json(input), Err(JcsError::NonCanonicalJson));
        }
    }

    #[test]
    fn accepts_canonical_primitives_and_nested_values() {
        for input in [
            b"null".as_slice(),
            b"true".as_slice(),
            b"0".as_slice(),
            br#"{"a":[false,null,{"b":"text"}]}"#.as_slice(),
        ] {
            validate_canonical_json(input).expect("input is canonical");
        }
    }

    #[test]
    fn canonicalization_is_idempotent_for_a_deterministic_corpus() {
        let corpus = [
            br#"{}"#.as_slice(),
            br#"[1,2,3]"#.as_slice(),
            br#"{"z":0,"a":-0.0}"#.as_slice(),
            br#"{"unicode":"\u20ac","nested":{"b":2,"a":1}}"#.as_slice(),
            br#"{"float":1.2345678901234567}"#.as_slice(),
        ];

        for input in corpus {
            let once = canonicalize_json(input).expect("valid corpus JSON");
            let twice = canonicalize_json(&once).expect("canonical output is valid JSON");
            assert_eq!(once, twice);
            validate_canonical_json(&once).expect("canonical output validates");
        }
    }
}
