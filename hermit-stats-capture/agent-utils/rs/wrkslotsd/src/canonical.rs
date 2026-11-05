use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::ObserverError;

/// Hash a JSON value using the compact, sorted, ASCII-only representation used
/// by the authoritative event writer.
pub fn canonical_sha256(value: &Value) -> Result<String, ObserverError> {
    let mut encoded = String::new();
    write_value(value, &mut encoded)?;
    Ok(format!("{:x}", Sha256::digest(encoded.as_bytes())))
}

pub(crate) fn canonical_json(value: &Value) -> Result<String, ObserverError> {
    let mut encoded = String::new();
    write_value(value, &mut encoded)?;
    Ok(encoded)
}

fn write_value(value: &Value, output: &mut String) -> Result<(), ObserverError> {
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(true) => output.push_str("true"),
        Value::Bool(false) => output.push_str("false"),
        Value::Number(number) => {
            output.push_str(&python_number(number));
        }
        Value::String(string) => write_string(string, output),
        Value::Array(values) => {
            output.push('[');
            for (index, item) in values.iter().enumerate() {
                if index != 0 {
                    output.push(',');
                }
                write_value(item, output)?;
            }
            output.push(']');
        }
        Value::Object(values) => {
            output.push('{');
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for (index, key) in keys.into_iter().enumerate() {
                if index != 0 {
                    output.push(',');
                }
                write_string(key, output);
                output.push(':');
                write_value(&values[key], output)?;
            }
            output.push('}');
        }
    }
    Ok(())
}

fn python_number(number: &serde_json::Number) -> String {
    let rendered = number.to_string();
    if number.is_i64() || number.is_u64() {
        return rendered;
    }
    if let Some((mantissa, exponent)) = rendered.split_once('e') {
        return format!("{mantissa}e{}", python_exponent(exponent));
    }
    let (sign, magnitude) = rendered
        .strip_prefix('-')
        .map_or(("", rendered.as_str()), |magnitude| ("-", magnitude));
    let Some(fraction) = magnitude.strip_prefix("0.") else {
        return rendered;
    };
    let zeroes = fraction.bytes().take_while(|byte| *byte == b'0').count();
    if zeroes < 4 || zeroes == fraction.len() {
        return rendered;
    }
    let digits = &fraction[zeroes..];
    let (first, rest) = digits.split_at(1);
    let mantissa = if rest.is_empty() {
        first.to_owned()
    } else {
        format!("{first}.{rest}")
    };
    let exponent = format!("-{}", zeroes + 1);
    format!("{sign}{mantissa}e{}", python_exponent(&exponent))
}

fn python_exponent(exponent: &str) -> String {
    let (sign, digits) = exponent.strip_prefix('-').map_or_else(
        || {
            exponent
                .strip_prefix('+')
                .map_or(("+", exponent), |digits| ("+", digits))
        },
        |digits| ("-", digits),
    );
    if digits.len() < 2 {
        format!("{sign}0{digits}")
    } else {
        format!("{sign}{digits}")
    }
}

fn write_string(value: &str, output: &mut String) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{0008}' => output.push_str("\\b"),
            '\u{000c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{0020}'..='\u{007e}' => output.push(character),
            character => {
                let mut utf16 = [0_u16; 2];
                for code_unit in character.encode_utf16(&mut utf16) {
                    output.push_str(&format!("\\u{code_unit:04x}"));
                }
            }
        }
    }
    output.push('"');
}
