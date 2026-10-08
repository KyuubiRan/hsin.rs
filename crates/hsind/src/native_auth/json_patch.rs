//! Replace only the selected root properties, preserving every other byte.

use jsonc_parser::{CollectOptions, ParseOptions, ast, common::Ranged, parse_to_ast};
use serde_json::Value;
use zeroize::Zeroizing;

use super::{Result, native_format_error};

pub(super) fn patch(before: &str, values: &Value, keys: &[&str]) -> Result<String> {
    let mut output = Zeroizing::new(if before.trim().is_empty() {
        "{}".into()
    } else {
        before.to_owned()
    });
    let values = values.as_object().ok_or_else(native_format_error)?;
    for key in keys {
        output = Zeroizing::new(set_property(&output, key, values.get(*key))?);
    }
    Ok(std::mem::take(&mut *output))
}

fn set_property(text: &str, key: &str, value: Option<&Value>) -> Result<String> {
    let parsed = parse_to_ast(text, &CollectOptions::default(), &ParseOptions::default())
        .map_err(|_| native_format_error())?;
    let Some(ast::Value::Object(object)) = parsed.value else {
        return Err(native_format_error());
    };
    // Ambiguous duplicate keys cannot be safely reconciled by a field allowlist.
    let matches: Vec<_> = object
        .properties
        .iter()
        .enumerate()
        .filter(|(_, property)| property.name.as_str() == key)
        .collect();
    if matches.len() > 1 {
        return Err(native_format_error());
    }
    if let Some((index, property)) = matches.first() {
        if let Some(value) = value {
            let raw = Zeroizing::new(serde_json::to_string(value)?);
            let range = property.value.range();
            return Ok(format!(
                "{}{}{}",
                &text[..range.start],
                raw.as_str(),
                &text[range.end..]
            ));
        }
        let mut output = text.to_owned();
        let following = object
            .properties
            .get(index + 1)
            .map_or(object.end() - 1, Ranged::start);
        let comma = comma_between(text, property.end(), following);
        if let Some(comma) = comma {
            output.remove(comma);
            output.replace_range(property.range.start..property.range.end, "");
        } else {
            output.replace_range(property.range.start..property.range.end, "");
            if *index > 0 {
                let previous = &object.properties[index - 1];
                if let Some(comma) = comma_between(text, previous.end(), property.start()) {
                    output.remove(comma);
                }
            }
        }
        return Ok(output);
    }
    let Some(value) = value else {
        return Ok(text.into());
    };
    let closing = object.end() - 1;
    let mut output = text.to_owned();
    let encoded = Zeroizing::new(serde_json::to_string(value)?);
    let raw = Zeroizing::new(format!(
        "{}{}: {}",
        line_ending(text),
        serde_json::to_string(key)?,
        encoded.as_str()
    ));
    if let Some(last) = object.properties.last()
        && comma_between(text, last.end(), closing).is_none()
    {
        output.insert(last.end(), ',');
        output.insert_str(closing + 1, &raw);
    } else {
        output.insert_str(closing, &raw);
    }
    Ok(output)
}

fn line_ending(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

fn comma_between(text: &str, start: usize, end: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut index = start;
    while index < end {
        match bytes[index] {
            b',' => return Some(index),
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index += 2;
                while index < end && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index + 1 < end && &bytes[index..index + 2] != b"*/" {
                    index += 1;
                }
                index += 2;
            }
            _ => index += 1,
        }
    }
    None
}
