use bytes::Bytes;
use chrono::{DateTime, NaiveDateTime, Utc};
use lookup::path;
use serde::{Deserialize, Serialize};
use smallvec::{smallvec, SmallVec};
use std::collections::HashMap;
use value::kind::Collection;
use value::Kind;
use vector_core::config::LogNamespace;
use vector_core::{
    config::{log_schema, DataType},
    event::Event,
    event::LogEvent,
    schema,
};

use super::Deserializer;
use crate::{gelf_fields::*, VALID_FIELD_REGEX};

/// On GELF decoding behavior:
///   Graylog has a relaxed decoding. They are much more lenient than the spec would
///   suggest. We've elected to take a more strict approach to maintain backwards compatability
///   in the event that we need to change the behavior to be more relaxed, so that prior versions
///   of vector will still work with the new relaxed decoding.

/// Config used to build a `GelfDeserializer`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GelfDeserializerConfig;

impl GelfDeserializerConfig {
    /// Build the `GelfDeserializer` from this configuration.
    pub fn build(&self) -> GelfDeserializer {
        GelfDeserializer::default()
    }

    /// Return the type of event built by this deserializer.
    pub fn output_type(&self) -> DataType {
        DataType::Log
    }

    /// The schema produced by the deserializer.
    pub fn schema_definition(&self, log_namespace: LogNamespace) -> schema::Definition {
        schema::Definition::new(Kind::object(Collection::empty()), [log_namespace])
            .with_field(VERSION, Kind::bytes(), None)
            .with_field(HOST, Kind::bytes(), None)
            .with_field(SHORT_MESSAGE, Kind::bytes(), None)
            .optional_field(FULL_MESSAGE, Kind::bytes(), None)
            .optional_field(TIMESTAMP, Kind::timestamp(), None)
            .optional_field(LEVEL, Kind::integer(), None)
            .optional_field(FACILITY, Kind::bytes(), None)
            .optional_field(LINE, Kind::integer(), None)
            .optional_field(FILE, Kind::bytes(), None)
            // Every field with an underscore (_) prefix will be treated as an additional field.
            // Allowed characters in field names are any word character (letter, number, underscore), dashes and dots.
            // Libraries SHOULD not allow to send id as additional field ( _id). Graylog server nodes omit this field automatically.
            .unknown_fields(Kind::bytes().or_integer().or_float())
    }
}

/// Deserializer that builds an `Event` from a byte frame containing a GELF log
/// message.
#[derive(Debug, Clone)]
pub struct GelfDeserializer;

impl Default for GelfDeserializer {
    fn default() -> Self {
        Self::new()
    }
}

impl GelfDeserializer {
    /// Create a new GelfDeserializer
    pub fn new() -> GelfDeserializer {
        GelfDeserializer
    }

    /// Builds a LogEvent from the parsed GelfMessage.
    /// The logic follows strictly the documented GELF standard.
    fn message_to_event(&self, parsed: &GelfMessage) -> vector_core::Result<Event> {
        let mut log = LogEvent::from_str_legacy(parsed.short_message.to_string());

        // GELF spec defines the version as 1.1 which has not changed since 2013
        if parsed.version != GELF_VERSION {
            return Err(format!(
                "{} does not match GELF spec version ({})",
                VERSION, GELF_VERSION
            )
            .into());
        }

        log.insert(VERSION, parsed.version.to_string());
        log.insert(HOST, parsed.host.to_string());

        if let Some(full_message) = &parsed.full_message {
            log.insert(FULL_MESSAGE, full_message.to_string());
        }

        if let Some(timestamp) = parsed.timestamp {
            let naive = NaiveDateTime::from_timestamp(
                f64::trunc(timestamp) as i64,
                f64::fract(timestamp) as u32,
            );
            log.insert(
                log_schema().timestamp_key(),
                DateTime::<Utc>::from_utc(naive, Utc),
            );
        // per GELF spec- add timestamp if not provided
        } else {
            log.insert(log_schema().timestamp_key(), Utc::now());
        }

        if let Some(level) = parsed.level {
            log.insert(LEVEL, level);
        }
        if let Some(facility) = &parsed.facility {
            log.insert(FACILITY, facility.to_string());
        }
        if let Some(line) = parsed.line {
            log.insert(
                LINE,
                value::Value::Float(
                    ordered_float::NotNan::new(line).expect("JSON doesn't allow NaNs"),
                ),
            );
        }
        if let Some(file) = &parsed.file {
            log.insert(FILE, file.to_string());
        }

        if let Some(add) = &parsed.additional_fields {
            for (key, val) in add.iter() {
                // per GELF spec, filter out _id
                if key == "_id" {
                    continue;
                }
                // per GELF spec, Additional field names must be prefixed with an underscore
                if !key.starts_with('_') {
                    return Err(format!(
                        "'{}' field is invalid. \
                                       Additional field names must be prefixed with an underscore.",
                        key
                    )
                    .into());
                }
                // per GELF spec, Additional field names must be characters dashes or dots
                if !VALID_FIELD_REGEX.is_match(key) {
                    return Err(format!("'{}' field contains invalid characters. Field names may \
                                       contain only letters, numbers, underscores, dashes and dots.", key).into());
                }

                // per GELF spec, Additional field values must be either strings or numbers
                if val.is_string() || val.is_number() {
                    let vector_val: value::Value = val.into();
                    log.insert(path!(key.as_str()), vector_val);
                } else {
                    let type_ = match val {
                        serde_json::Value::Null => "null",
                        serde_json::Value::Bool(_) => "boolean",
                        serde_json::Value::Number(_) => "number",
                        serde_json::Value::String(_) => "string",
                        serde_json::Value::Array(_) => "array",
                        serde_json::Value::Object(_) => "object",
                    };
                    return Err(format!("The value type for field {} is an invalid type ({}). Additional field values \
                                       should be either strings or numbers.", key, type_).into());
                }
            }
        }
        Ok(Event::Log(log))
    }
}

#[derive(Serialize, Deserialize, Debug)]
struct GelfMessage {
    version: String,
    host: String,
    short_message: String,
    full_message: Option<String>,
    timestamp: Option<f64>,
    level: Option<u8>,
    facility: Option<String>,
    line: Option<f64>,
    file: Option<String>,
    #[serde(flatten)]
    additional_fields: Option<HashMap<String, serde_json::Value>>,
}

impl Deserializer for GelfDeserializer {
    fn parse(
        &self,
        bytes: Bytes,
        _log_namespace: LogNamespace,
    ) -> vector_core::Result<SmallVec<[Event; 1]>> {
        let line = std::str::from_utf8(&bytes)?;
        let line = line.trim();

        let parsed: GelfMessage = serde_json::from_str(line)?;
        let event = self.message_to_event(&parsed)?;

        Ok(smallvec![event])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use chrono::{DateTime, NaiveDateTime, Utc};
    use lookup::path;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use smallvec::SmallVec;
    use value::Value;
    use vector_core::{config::log_schema, event::Event};

    fn deserialize_gelf_input(
        input: &serde_json::Value,
    ) -> vector_core::Result<SmallVec<[Event; 1]>> {
        let config = GelfDeserializerConfig;
        let deserializer = config.build();
        let buffer = Bytes::from(serde_json::to_vec(&input).unwrap());
        deserializer.parse(buffer, LogNamespace::Legacy)
    }

    /// Validates all the spec'd fields of GELF are deserialized correctly.
    #[test]
    fn gelf_deserialize_correctness() {
        let add_on_int_in = "_an.add-field_int";
        let add_on_str_in = "_an.add-field_str";

        let input = json!({
            VERSION: "1.1",
            HOST: "example.org",
            SHORT_MESSAGE: "A short message that helps you identify what is going on",
            FULL_MESSAGE: "Backtrace here\n\nmore stuff",
            TIMESTAMP: 1385053862.3072,
            LEVEL: 1,
            FACILITY: "foo",
            LINE: 42,
            FILE: "/tmp/bar",
            add_on_int_in: 2001.1002,
            add_on_str_in: "A Space Odyssey",
        });

        // Ensure that we can parse the gelf json successfully
        let events = deserialize_gelf_input(&input).unwrap();
        assert_eq!(events.len(), 1);

        let log = events[0].as_log();

        assert_eq!(
            log.get(VERSION),
            Some(&Value::Bytes(Bytes::from_static(b"1.1")))
        );
        assert_eq!(
            log.get(HOST),
            Some(&Value::Bytes(Bytes::from_static(b"example.org")))
        );
        assert_eq!(
            log.get(log_schema().message_key()),
            Some(&Value::Bytes(Bytes::from_static(
                b"A short message that helps you identify what is going on"
            )))
        );
        assert_eq!(
            log.get(FULL_MESSAGE),
            Some(&Value::Bytes(Bytes::from_static(
                b"Backtrace here\n\nmore stuff"
            )))
        );
        // Vector does not use the nanos
        let naive = NaiveDateTime::from_timestamp(1385053862, 0);
        assert_eq!(
            log.get(TIMESTAMP),
            Some(&Value::Timestamp(DateTime::<Utc>::from_utc(naive, Utc)))
        );
        assert_eq!(log.get(LEVEL), Some(&Value::Integer(1)));
        assert_eq!(
            log.get(FACILITY),
            Some(&Value::Bytes(Bytes::from_static(b"foo")))
        );
        assert_eq!(
            log.get(LINE),
            Some(&Value::Float(ordered_float::NotNan::new(42.0).unwrap()))
        );
        assert_eq!(
            log.get(FILE),
            Some(&Value::Bytes(Bytes::from_static(b"/tmp/bar")))
        );
        assert_eq!(
            log.get(path!(add_on_int_in)),
            Some(&Value::Float(
                ordered_float::NotNan::new(2001.1002).unwrap()
            ))
        );
        assert_eq!(
            log.get(path!(add_on_str_in)),
            Some(&Value::Bytes(Bytes::from_static(b"A Space Odyssey")))
        );
    }

    /// Validates deserializiation succeeds for edge case inputs.
    #[test]
    fn gelf_deserializing_edge_cases() {
        // timestamp is set if omitted from input
        {
            let input = json!({
                HOST: "example.org",
                SHORT_MESSAGE: "foobar",
                VERSION: "1.1",
            });
            let events = deserialize_gelf_input(&input).unwrap();
            assert_eq!(events.len(), 1);
            let log = events[0].as_log();
            assert!(log.contains(log_schema().message_key()));
        }

        // filter out id
        {
            let input = json!({
                HOST: "example.org",
                SHORT_MESSAGE: "foobar",
                VERSION: "1.1",
                "_id": "S3creTz",
            });
            let events = deserialize_gelf_input(&input).unwrap();
            assert_eq!(events.len(), 1);
            let log = events[0].as_log();
            assert!(!log.contains("_id"));
        }
    }

    /// Validates the error conditions in deserialization
    #[test]
    fn gelf_deserializing_err() {
        fn validate_err(input: &serde_json::Value) {
            assert!(deserialize_gelf_input(input).is_err());
        }
        //  invalid character in field name
        validate_err(&json!({
            HOST: "example.org",
            SHORT_MESSAGE: "foobar",
            VERSION: "1.1",
            "_bad%key": "raboof",
        }));

        //  not prefixed with underscore
        validate_err(&json!({
            HOST: "example.org",
            SHORT_MESSAGE: "foobar",
            VERSION: "1.1",
            "bad-key": "raboof",
        }));

        // missing short_message
        validate_err(&json!({
            HOST: "example.org",
            VERSION: "1.1",
        }));

        // host is not specified
        validate_err(&json!({
            SHORT_MESSAGE: "foobar",
            VERSION: "1.1",
        }));

        // host is not a string
        validate_err(&json!({
            HOST: 42,
            SHORT_MESSAGE: "foobar",
            VERSION: "1.1",
        }));

        //  level / line is string and not numeric
        validate_err(&json!({
            HOST: "example.org",
            VERSION: "1.1",
            SHORT_MESSAGE: "foobar",
            LEVEL: "baz",
        }));
    }
}
