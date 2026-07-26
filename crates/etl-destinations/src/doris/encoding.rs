//! Cell-to-JSON encoding for Doris Stream Load.

use etl::data::{ArrayCell, Cell};
use serde_json::{Number, Value};

/// Timestamp format accepted by Doris `datetime(6)`.
const DATETIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S%.6f";

/// Converts a [`Cell`] into a JSON value for Stream Load.
pub(super) fn cell_to_json(cell: &Cell) -> Value {
    match cell {
        Cell::Null => Value::Null,
        Cell::Bool(v) => Value::Bool(*v),
        Cell::String(v) => Value::String(v.clone()),
        Cell::I16(v) => Value::Number(Number::from(*v)),
        Cell::I32(v) => Value::Number(Number::from(*v)),
        Cell::U32(v) => Value::Number(Number::from(*v)),
        Cell::I64(v) => Value::Number(Number::from(*v)),
        Cell::F32(v) => float_to_json(f64::from(*v)),
        Cell::F64(v) => float_to_json(*v),
        Cell::Numeric(v) => Value::String(v.to_string()),
        Cell::Date(v) => Value::String(v.to_string()),
        Cell::Time(v) => Value::String(v.to_string()),
        Cell::TimeTz(v) => Value::String(v.to_string()),
        Cell::Timestamp(v) => Value::String(v.format(DATETIME_FORMAT).to_string()),
        Cell::TimestampTz(v) => Value::String(v.naive_utc().format(DATETIME_FORMAT).to_string()),
        Cell::Uuid(v) => Value::String(v.to_string()),
        Cell::Json(v) => v.clone(),
        Cell::Bytes(v) => Value::String(hex_encode(v)),
        Cell::Array(v) => Value::String(array_to_text(v)),
    }
}

/// Renders a float, falling back to string for non-finite values.
fn float_to_json(value: f64) -> Value {
    Number::from_f64(value).map_or_else(|| Value::String(value.to_string()), Value::Number)
}

/// Hex-encodes bytes.
fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Converts a Postgres array cell to a text representation for varchar columns.
fn array_to_text(array: &ArrayCell) -> String {
    match array {
        ArrayCell::Bool(v) => format_array(v),
        ArrayCell::String(v) => format_array(v),
        ArrayCell::I16(v) => format_array(v),
        ArrayCell::I32(v) => format_array(v),
        ArrayCell::U32(v) => format_array(v),
        ArrayCell::I64(v) => format_array(v),
        ArrayCell::F32(v) => format_array(v),
        ArrayCell::F64(v) => format_array(v),
        ArrayCell::Numeric(v) => format_array(v),
        ArrayCell::Date(v) => format_array(v),
        ArrayCell::Time(v) => format_array(v),
        ArrayCell::TimeTz(v) => format_array(v),
        ArrayCell::Timestamp(v) => format_array(v),
        ArrayCell::TimestampTz(v) => format_array(v),
        ArrayCell::Uuid(v) => format_array(v),
        ArrayCell::Json(v) => format_array(v),
        ArrayCell::Bytes(v) => format_array(v),
    }
}

/// Formats a nullable-element vector into `{elem1,elem2,...}` text.
fn format_array<T: std::fmt::Debug>(elements: &[Option<T>]) -> String {
    let inner: Vec<String> = elements
        .iter()
        .map(|e| match e {
            Some(v) => format!("{v:?}"),
            None => "NULL".to_owned(),
        })
        .collect();
    format!("{{{}}}", inner.join(","))
}

#[cfg(test)]
mod tests {
    use chrono::{NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
    use uuid::Uuid;

    use super::*;

    #[test]
    fn null_encodes_to_null() {
        assert_eq!(cell_to_json(&Cell::Null), Value::Null);
    }

    #[test]
    fn bool_encodes() {
        assert_eq!(cell_to_json(&Cell::Bool(true)), Value::Bool(true));
    }

    #[test]
    fn integers_encode() {
        assert_eq!(cell_to_json(&Cell::I32(42)), Value::Number(42.into()));
    }

    #[test]
    fn timestamp_encodes() {
        let ts = NaiveDateTime::new(
            NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
            NaiveTime::from_hms_micro_opt(10, 30, 0, 123_456).unwrap(),
        );
        let v = cell_to_json(&Cell::Timestamp(ts));
        assert_eq!(v, Value::String("2024-01-15 10:30:00.123456".to_owned()));
    }

    #[test]
    fn timestamptz_encodes_as_utc() {
        let ts = Utc.with_ymd_and_hms(2024, 6, 1, 12, 0, 0).unwrap();
        let v = cell_to_json(&Cell::TimestampTz(ts));
        assert_eq!(v, Value::String("2024-06-01 12:00:00.000000".to_owned()));
    }

    #[test]
    fn uuid_encodes_as_string() {
        let u = Uuid::nil();
        let v = cell_to_json(&Cell::Uuid(u));
        assert_eq!(v, Value::String("00000000-0000-0000-0000-000000000000".to_owned()));
    }

    #[test]
    fn bytes_encode_as_hex() {
        let v = cell_to_json(&Cell::Bytes(vec![0xde, 0xad]));
        assert_eq!(v, Value::String("dead".to_owned()));
    }
}
