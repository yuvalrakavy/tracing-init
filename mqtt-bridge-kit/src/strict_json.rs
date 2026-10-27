//! Opt-in ordinary-request validation before generic JSON decoding loses keys.
#[cfg(test)]
mod runtime_tests;

use serde::{de, Deserialize};
use std::{collections::BTreeSet, fmt};

struct Checked;
impl<'de> Deserialize<'de> for Checked {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(Check)
    }
}

struct Check;
impl<'de> de::Visitor<'de> for Check {
    type Value = Checked;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON without duplicate object keys")
    }
    fn visit_unit<E: de::Error>(self) -> Result<Checked, E> {
        Ok(Checked)
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Checked, E> {
        Ok(Checked)
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Checked, E> {
        Ok(Checked)
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Checked, E> {
        Ok(Checked)
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Checked, E> {
        Ok(Checked)
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<Checked, E> {
        Ok(Checked)
    }
    fn visit_seq<A: de::SeqAccess<'de>>(self, mut sequence: A) -> Result<Checked, A::Error> {
        while sequence.next_element::<Checked>()?.is_some() {}
        Ok(Checked)
    }
    fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Checked, A::Error> {
        // Only decoded keys survive this pass, scoped to each object. Values
        // and arrays are visited without building a second JSON representation.
        let mut keys = BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key) {
                return Err(de::Error::custom("duplicate JSON object key"));
            }
            map.next_value::<Checked>()?;
        }
        Ok(Checked)
    }
}

pub(crate) fn validate(payload: &[u8]) -> Result<(), String> {
    let mut decoder = serde_json::Deserializer::from_slice(payload);
    Checked::deserialize(&mut decoder).map_err(|error| format!("not strict JSON: {error}"))?;
    decoder.end().map_err(|error| format!("not strict JSON: {error}"))
}

pub(crate) fn decode<T>(strict: bool, payload: &[u8], decoder: impl FnOnce(&[u8]) -> Result<T, String>) -> Result<T, String> {
    if strict && !payload.is_empty() {
        validate(payload)?;
    }
    decoder(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_keys_are_rejected_before_generic_configuration_decoding() {
        let raw = br#"{"revision":"1","revision":"2"}"#;
        assert_eq!(
            crate::wire::decode_config(raw).unwrap().unwrap()["revision"],
            "2",
            "probe must demonstrate information lost by the legacy decoder"
        );
        assert!(validate(raw).is_err(), "strict JSON accepted duplicate configuration keys");
    }

    #[test]
    fn nested_and_escaped_duplicate_keys_are_rejected() {
        for raw in [br#"{"arrays":[{"x":1,"x":2}]}"#.as_slice(), br#"{"x":1,"\u0078":2}"#] {
            assert!(validate(raw).is_err(), "strict JSON accepted nested or escaped duplicate keys");
        }
    }

    #[test]
    fn strict_mode_preserves_empty_retractions() {
        assert_eq!(decode(true, b"", crate::wire::decode_config).unwrap(), None);
        assert_eq!(decode(true, b"", crate::wire::decode_desired_property).unwrap(), None);
        assert!(decode(true, b"", crate::wire::decode_command).is_err());
    }

    #[test]
    fn sibling_keys_scalars_and_empty_documents_keep_their_grammar() {
        for raw in [br#"[{"x":1},{"x":2}]"#.as_slice(), b"null", b"true", b"12.5", br#""text""#, b"{}", b"[]"] {
            validate(raw).unwrap();
        }
        for raw in [b"true false".as_slice(), b"{", b"[1,]", b"NaN", &[0xff]] {
            assert!(validate(raw).is_err(), "strict JSON accepted malformed input");
        }
    }
}
