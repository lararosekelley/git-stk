//! Shared parsing helpers for provider CLI JSON output.

use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::{ReviewRequest, ReviewState};

pub(super) fn parse_body_field(output: &str, field: &str) -> Result<String> {
    let value: serde_json::Value =
        serde_json::from_str(output).context("failed to parse provider JSON")?;
    Ok(value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned())
}

pub(super) fn optional_bool(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

pub(super) fn optional_string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

pub(super) fn first_json_item(output: &str) -> Result<Option<Value>> {
    let value: Value = serde_json::from_str(output).context("failed to parse provider JSON")?;
    match value {
        Value::Array(items) => Ok(items.into_iter().next()),
        Value::Object(_) => Ok(Some(value)),
        _ => bail!("provider JSON must be an object or array"),
    }
}

/// Every item in a JSON array (or the single object as a one-element list).
pub(super) fn json_items(output: &str) -> Result<Vec<Value>> {
    let value: Value = serde_json::from_str(output).context("failed to parse provider JSON")?;
    match value {
        Value::Array(items) => Ok(items),
        Value::Object(_) => Ok(vec![value]),
        _ => bail!("provider JSON must be an object or array"),
    }
}

/// The first review in the provider's JSON, with its fields mapped by the
/// provider-specific `from`. The array/object shell is shared; only `from`
/// (the field names) differs between GitHub and GitLab.
pub(super) fn first_review<F>(output: &str, from: F) -> Result<Option<ReviewRequest>>
where
    F: Fn(&Value) -> Result<ReviewRequest>,
{
    first_json_item(output)?.as_ref().map(from).transpose()
}

/// Every review in the provider's JSON, mapped by the provider-specific `from`.
pub(super) fn all_reviews<F>(output: &str, from: F) -> Result<Vec<ReviewRequest>>
where
    F: Fn(&Value) -> Result<ReviewRequest>,
{
    json_items(output)?.iter().map(from).collect()
}

pub(super) fn required_string(value: &Value, keys: &[&str]) -> Result<String> {
    for key in keys {
        if let Some(field) = value.get(*key) {
            if let Some(value) = field.as_str() {
                return Ok(value.to_owned());
            }
            if let Some(value) = field.as_i64() {
                return Ok(value.to_string());
            }
            if let Some(value) = field.as_u64() {
                return Ok(value.to_string());
            }
        }
    }

    bail!(
        "provider JSON missing required field: {}",
        keys.join(" or ")
    )
}

pub(super) fn parse_state(state: &str) -> ReviewState {
    match state.to_ascii_lowercase().as_str() {
        "open" | "opened" => ReviewState::Open,
        "merged" => ReviewState::Merged,
        "closed" => ReviewState::Closed,
        _ => ReviewState::Unknown(state.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_body_field_reads_field_and_defaults_empty() {
        assert_eq!(
            parse_body_field(r#"{"body":"hello"}"#, "body").expect("parse body"),
            "hello"
        );
        assert_eq!(
            parse_body_field(r#"{"description":null}"#, "description").expect("parse body"),
            ""
        );
        assert_eq!(parse_body_field(r#"{}"#, "body").expect("parse body"), "");
    }
}
