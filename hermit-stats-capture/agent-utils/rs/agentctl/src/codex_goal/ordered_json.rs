//! JSON read in document order and written in the native goal command's output format.
//!
//! The command prints the goal the Codex server returned with three properties
//! `serde_json::Value` does not promise: object members stay in document order (whatever the
//! `preserve_order` feature unification of a workspace build says), a repeated key keeps its
//! first position and its last value, and output separates items with `, ` and keys with `: `,
//! writing non-ASCII text as is.

// These are the conventions of json.loads and json.dumps(value, ensure_ascii=False), which the
// other edition's codex_goal module used; its output and messages are reproduced byte for byte.

use std::fmt::{self, Write as _};

use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

/// One decoded JSON value whose objects keep their members in document order.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Str(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    /// Decode one JSON document.
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    /// Build an object from members in the order given.
    pub(crate) fn object<const N: usize>(members: [(&str, Json); N]) -> Self {
        Self::Object(
            members
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
        )
    }

    /// Append a member to an object; other values are unchanged.
    pub(crate) fn push(&mut self, key: &str, value: Json) {
        if let Self::Object(members) = self {
            members.push((key.to_owned(), value));
        }
    }

    /// The member `key` of an object.
    pub(crate) fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Self::Object(members) => members
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    /// Whether an object has the member `key`.
    pub(crate) fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(value) => Some(value),
            _ => None,
        }
    }

    pub(crate) fn as_array(&self) -> Option<&[Json]> {
        match self {
            Self::Array(values) => Some(values),
            _ => None,
        }
    }

    pub(crate) fn is_object(&self) -> bool {
        matches!(self, Self::Object(_))
    }

    pub(crate) fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// Whether this is an integer; JSON booleans and floats are not.
    pub(crate) fn is_int(&self) -> bool {
        matches!(self, Self::Int(_) | Self::UInt(_))
    }

    /// Whether this is the integer `expected`.
    pub(crate) fn is_int_equal(&self, expected: i64) -> bool {
        match self {
            Self::Int(value) => *value == expected,
            Self::UInt(value) => i64::try_from(*value) == Ok(expected),
            _ => false,
        }
    }

    /// The type name messages use for the value: `NoneType`, `bool`, `int`, `float`, `str`, `list` or `dict`.
    pub(crate) fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "NoneType",
            Self::Bool(_) => "bool",
            Self::Int(_) | Self::UInt(_) => "int",
            Self::Float(_) => "float",
            Self::Str(_) => "str",
            Self::Array(_) => "list",
            Self::Object(_) => "dict",
        }
    }

    /// The text `json.dumps(value, ensure_ascii=False)` produces.
    pub(crate) fn dumps(&self) -> String {
        let mut output = String::new();
        self.write_json(&mut output);
        output
    }

    fn write_json(&self, output: &mut String) {
        match self {
            Self::Null => output.push_str("null"),
            Self::Bool(true) => output.push_str("true"),
            Self::Bool(false) => output.push_str("false"),
            Self::Int(value) => {
                let _ = write!(output, "{value}");
            }
            Self::UInt(value) => {
                let _ = write!(output, "{value}");
            }
            Self::Float(value) => output.push_str(&json_float(*value)),
            Self::Str(value) => write_json_string(value, output),
            Self::Array(values) => {
                output.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        output.push_str(", ");
                    }
                    value.write_json(output);
                }
                output.push(']');
            }
            Self::Object(members) => {
                output.push('{');
                for (index, (key, value)) in members.iter().enumerate() {
                    if index > 0 {
                        output.push_str(", ");
                    }
                    write_json_string(key, output);
                    output.push_str(": ");
                    value.write_json(output);
                }
                output.push('}');
            }
        }
    }

    /// The value as a message quotes it: a string as is, anything else in repr form.
    pub(crate) fn message_text(&self) -> String {
        match self {
            Self::Str(value) => value.clone(),
            _ => self.repr_text(),
        }
    }

    /// The repr form: `None`, `True`, `False`, numbers, `[a, b]` and `{'k': v}`. Strings take
    /// single quotes unless they hold a single quote and no double quote, and escape that quote,
    /// backslashes, and C0, DEL and C1 control characters; other characters are written as is.
    fn repr_text(&self) -> String {
        match self {
            Self::Null => "None".to_owned(),
            Self::Bool(true) => "True".to_owned(),
            Self::Bool(false) => "False".to_owned(),
            Self::Int(value) => value.to_string(),
            Self::UInt(value) => value.to_string(),
            Self::Float(value) => float_repr(*value),
            Self::Str(value) => quoted_repr(value),
            Self::Array(values) => format!(
                "[{}]",
                values
                    .iter()
                    .map(Self::repr_text)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Object(members) => format!(
                "{{{}}}",
                members
                    .iter()
                    .map(|(key, value)| format!("{}: {}", quoted_repr(key), value.repr_text()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    /// The same value as a `serde_json::Value`, for callers of the crate API.
    pub(crate) fn to_value(&self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(value) => Value::Bool(*value),
            Self::Int(value) => Value::Number((*value).into()),
            Self::UInt(value) => Value::Number((*value).into()),
            Self::Float(value) => Number::from_f64(*value).map_or(Value::Null, Value::Number),
            Self::Str(value) => Value::String(value.clone()),
            Self::Array(values) => Value::Array(values.iter().map(Self::to_value).collect()),
            Self::Object(members) => {
                let mut map = Map::new();
                for (key, value) in members {
                    map.insert(key.clone(), value.to_value());
                }
                Value::Object(map)
            }
        }
    }
}

fn write_json_string(value: &str, output: &mut String) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{8}' => output.push_str("\\b"),
            '\u{c}' => output.push_str("\\f"),
            control if u32::from(control) < 0x20 => {
                let _ = write!(output, "\\u{:04x}", u32::from(control));
            }
            other => output.push(other),
        }
    }
    output.push('"');
}

fn quoted_repr(value: &str) -> String {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut output = String::new();
    output.push(quote);
    for character in value.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            other if other == quote => {
                output.push('\\');
                output.push(other);
            }
            control if u32::from(control) < 0x20 || (0x7f..=0x9f).contains(&u32::from(control)) => {
                let _ = write!(output, "\\x{:02x}", u32::from(control));
            }
            other => output.push(other),
        }
    }
    output.push(quote);
    output
}

/// A float in the output format: [`float_repr`], with `NaN` and `Infinity` for non-finite values.
fn json_float(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_owned()
    } else if value.is_infinite() {
        if value > 0.0 { "Infinity" } else { "-Infinity" }.to_owned()
    } else {
        float_repr(value)
    }
}

/// The shortest digits that round-trip, in fixed notation (`100.0`, `0.0001`) when the
/// decimal exponent is from -4 through 15 and in exponent notation (`1e+16`, `1.5e-05`) otherwise.
pub(crate) fn float_repr(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.to_owned();
    }
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0"
        } else {
            "0.0"
        }
        .to_owned();
    }
    let (negative, digits, exponent) = shortest_digits(value);
    let mut output = String::new();
    if negative {
        output.push('-');
    }
    if (-4..=15).contains(&exponent) {
        output.push_str(&fixed(&digits, exponent, true));
    } else {
        output.push_str(&digits[..1]);
        if digits.len() > 1 {
            output.push('.');
            output.push_str(&digits[1..]);
        }
        push_exponent(&mut output, exponent);
    }
    output
}

/// The `%g` format: six significant digits, trailing zeros removed, exponent
/// notation when the decimal exponent is below -4 or at least 6.
pub(crate) fn format_g(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.to_owned();
    }
    if value == 0.0 {
        return if value.is_sign_negative() { "-0" } else { "0" }.to_owned();
    }
    let rounded = format!("{:.5e}", value.abs());
    let (mantissa, exponent) = rounded
        .split_once('e')
        .expect("Rust exponent formatting has an exponent");
    let exponent: i32 = exponent.parse().expect("Rust exponent is an integer");
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let mut output = String::new();
    if value.is_sign_negative() {
        output.push('-');
    }
    if (-4..6).contains(&exponent) {
        output.push_str(&fixed(digits, exponent, false));
    } else {
        output.push_str(&digits[..1]);
        if digits.len() > 1 {
            output.push('.');
            output.push_str(&digits[1..]);
        }
        push_exponent(&mut output, exponent);
    }
    output
}

/// Shortest round-trip decimal digits of a finite nonzero value and its decimal exponent.
fn shortest_digits(value: f64) -> (bool, String, i32) {
    // Rust's `{:e}` without a precision prints the shortest digits that round-trip, as
    // the other edition's repr does: `1.2345e2`, `1e-5`.
    let text = format!("{:e}", value.abs());
    let (mantissa, exponent) = text
        .split_once('e')
        .expect("Rust exponent formatting has an exponent");
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    (
        value.is_sign_negative(),
        digits.to_owned(),
        exponent.parse().expect("Rust exponent is an integer"),
    )
}

/// Digits `d1d2...` with decimal exponent `exponent`, written in fixed notation. With
/// `point_zero` an integral value keeps `.0`, as `repr` writes it; otherwise it has no point.
fn fixed(digits: &str, exponent: i32, point_zero: bool) -> String {
    let mut output = String::new();
    if exponent < 0 {
        output.push_str("0.");
        for _ in 0..(-exponent - 1) {
            output.push('0');
        }
        output.push_str(digits);
        return output;
    }
    let integer_digits = usize::try_from(exponent).expect("nonnegative exponent") + 1;
    if digits.len() <= integer_digits {
        output.push_str(digits);
        for _ in digits.len()..integer_digits {
            output.push('0');
        }
        if point_zero {
            output.push_str(".0");
        }
    } else {
        output.push_str(&digits[..integer_digits]);
        output.push('.');
        output.push_str(&digits[integer_digits..]);
    }
    output
}

fn push_exponent(output: &mut String, exponent: i32) {
    let _ = write!(
        output,
        "e{}{:02}",
        if exponent < 0 { '-' } else { '+' },
        exponent.unsigned_abs()
    );
}

/// Strip surrounding Unicode whitespace, including the four
/// information separators U+001C to U+001F that Rust's `char::is_whitespace` omits.
pub(crate) fn strip_space(value: &str) -> &str {
    value.trim_matches(is_space)
}

/// Whether `character` is whitespace in the sense of [`strip_space`].
pub(crate) fn is_space(character: char) -> bool {
    character.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&character)
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(JsonVisitor)
    }
}

struct JsonVisitor;

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = Json;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Json, E> {
        Ok(Json::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Json, E> {
        Ok(Json::Int(value))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Json, E> {
        Ok(Json::UInt(value))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Json, E> {
        Ok(Json::Float(value))
    }

    fn visit_str<E>(self, value: &str) -> Result<Json, E> {
        Ok(Json::Str(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Json, E> {
        Ok(Json::Str(value))
    }

    fn visit_unit<E>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_none<E>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Json, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element()? {
            values.push(value);
        }
        Ok(Json::Array(values))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
        let mut members: Vec<(String, Json)> = Vec::new();
        while let Some((key, value)) = map.next_entry::<String, Json>()? {
            // As a dict is built: a repeated key keeps its first position and its last value.
            if let Some(existing) = members.iter_mut().find(|(name, _)| *name == key) {
                existing.1 = value;
            } else {
                members.push((key, value));
            }
        }
        Ok(Json::Object(members))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dumps_matches_python_separators_order_and_escapes() {
        let value = Json::parse(
            concat!(
                r#"{"z": 1, "a": [true, null, -2, 18446744073709551615], "#,
                r#""s": "é\u2028\u0001\"\\\n\/", "a": {"k": 1.5}}"#
            )
            .as_bytes(),
        )
        .unwrap();
        // json.dumps(json.loads(same), ensure_ascii=False)
        assert_eq!(
            value.dumps(),
            "{\"z\": 1, \"a\": {\"k\": 1.5}, \"s\": \"\u{e9}\u{2028}\\u0001\\\"\\\\\\n/\"}"
        );
        assert_eq!(Json::parse(b"[]").unwrap().dumps(), "[]");
        assert_eq!(Json::parse(b"{}").unwrap().dumps(), "{}");
        assert_eq!(
            Json::parse(b"[\"\\b\\f\\t\\r\\u007f\"]").unwrap().dumps(),
            "[\"\\b\\f\\t\\r\u{7f}\"]"
        );
    }

    #[test]
    fn float_repr_matches_python() {
        for (value, expected) in [
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.0, "1.0"),
            (10.0, "10.0"),
            (1.5, "1.5"),
            (0.1, "0.1"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1.5e-7, "1.5e-07"),
            (123_456_789_012_345.6, "123456789012345.6"),
            (1e15, "1000000000000000.0"),
            (1e16, "1e+16"),
            (1.5e16, "1.5e+16"),
            (1e100, "1e+100"),
            (-2.5e-300, "-2.5e-300"),
            (f64::MAX, "1.7976931348623157e+308"),
        ] {
            assert_eq!(float_repr(value), expected, "{value:e}");
        }
        assert_eq!(Json::Float(f64::INFINITY).dumps(), "Infinity");
        assert_eq!(Json::Float(f64::NAN).dumps(), "NaN");
    }

    #[test]
    fn format_g_matches_python() {
        for (value, expected) in [
            (30.0, "30"),
            (0.5, "0.5"),
            (0.05, "0.05"),
            (1.0 / 3.0, "0.333333"),
            (100_000.0, "100000"),
            (1_000_000.0, "1e+06"),
            (1_234_567.0, "1.23457e+06"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (9.999_996, "10"),
            (2.5e-10, "2.5e-10"),
            (1e300, "1e+300"),
        ] {
            assert_eq!(format_g(value), expected, "{value:e}");
        }
    }

    #[test]
    fn repeated_keys_keep_first_position_and_last_value() {
        let value = Json::parse(br#"{"a": 1, "b": 2, "a": 3}"#).unwrap();
        assert_eq!(value.dumps(), r#"{"a": 3, "b": 2}"#);
        assert!(value.get("a").unwrap().is_int_equal(3));
    }

    #[test]
    fn python_str_and_type_names_follow_python() {
        let value =
            Json::parse(br#"{"code": -1, "data": [null, true, "it's", "say \"x\""]}"#).unwrap();
        assert_eq!(
            value.message_text(),
            r#"{'code': -1, 'data': [None, True, "it's", 'say "x"']}"#
        );
        assert_eq!(Json::Str("plain".to_owned()).message_text(), "plain");
        assert_eq!(
            Json::Str("a'b\"c\n".to_owned()).repr_text(),
            r#"'a\'b"c\n'"#
        );
        for (text, name) in [
            ("null", "NoneType"),
            ("true", "bool"),
            ("1", "int"),
            ("-1", "int"),
            ("1.0", "float"),
            ("\"\"", "str"),
            ("[]", "list"),
            ("{}", "dict"),
        ] {
            assert_eq!(Json::parse(text.as_bytes()).unwrap().type_name(), name);
        }
    }

    #[test]
    fn python_strip_includes_information_separators() {
        assert_eq!(strip_space("\u{1c}\u{1f} \t text \u{3000}\n"), "text");
        assert_eq!(strip_space("\u{1d}"), "");
    }
}
