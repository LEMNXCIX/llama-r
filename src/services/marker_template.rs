//! Marker-template rendering over JSON payloads.
//!
//! A webhook channel may declare a `template` in which `{{path.to.field}}`
//! markers are replaced with values from the inbound JSON payload. This is a
//! deliberately tiny renderer: no dependency, no expression language and no
//! HTML escaping (the result becomes a user message, not markup). The only
//! syntax is a `{{` ... `}}` pair whose contents are a dot-separated path into
//! the payload.
//!
//! Failures are *named*: an unknown path or a non-scalar value produces an
//! error that mentions only the path taken from the template — never the
//! payload body — so a webhook carrying a secret can never have that secret
//! echoed back in an error message.

use serde_json::Value;
use std::borrow::Cow;

/// Renders `template` by replacing each `{{path.to.field}}` marker with the
/// scalar value found at that dot-path in `payload`.
///
/// - strings render as themselves
/// - numbers and booleans render as their text form
/// - objects, arrays and null are named errors (they have no text form)
/// - a missing/unknown path is a named error mentioning only the path
///
/// A template with no markers is returned verbatim. Body content is never
/// included in an error: errors name the path from the template only.
pub fn render_markers(template: &str, payload: &Value) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(open) = rest.find("{{") {
        // Everything before the marker is literal text.
        out.push_str(&rest[..open]);
        let after_open = &rest[open + 2..];

        let Some(close) = after_open.find("}}") else {
            return Err("malformed_marker: unclosed '{{' in template".to_string());
        };

        let marker = after_open[..close].trim();
        let value = resolve_path(payload, marker)?;
        out.push_str(&scalar_text(value, marker)?);

        rest = &after_open[close + 2..];
    }

    out.push_str(rest);
    Ok(out)
}

/// Walks `path` (dot-separated, each segment trimmed) from `payload`, returning
/// the value found. Errors name only `path`, never any payload content.
fn resolve_path<'a>(payload: &'a Value, path: &str) -> Result<&'a Value, String> {
    let mut current = payload;
    for segment in path.split('.').map(str::trim) {
        current = current
            .get(segment)
            .ok_or_else(|| format!("unknown_marker: no value at path '{path}'"))?;
    }
    Ok(current)
}

/// Converts a resolved value to its message text. Non-scalar values are named
/// errors that include only `path`, never the value itself.
fn scalar_text<'a>(value: &'a Value, path: &str) -> Result<Cow<'a, str>, String> {
    match value {
        Value::String(s) => Ok(Cow::Borrowed(s.as_str())),
        Value::Number(n) => Ok(Cow::Owned(n.to_string())),
        Value::Bool(b) => Ok(Cow::Owned(b.to_string())),
        Value::Object(_) => Err(format!(
            "unsupported_marker_value: path '{path}' resolves to an object, which cannot be rendered as text"
        )),
        Value::Array(_) => Err(format!(
            "unsupported_marker_value: path '{path}' resolves to an array, which cannot be rendered as text"
        )),
        Value::Null => Err(format!(
            "unsupported_marker_value: path '{path}' resolves to null, which cannot be rendered as text"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn no_marker_is_verbatim() {
        let payload = json!({"a": 1});
        assert_eq!(
            render_markers("plain text", &payload).unwrap(),
            "plain text"
        );
    }

    #[test]
    fn unclosed_marker_is_named_error() {
        let payload = json!({"a": 1});
        let err = render_markers("value {{a", &payload).unwrap_err();
        assert!(err.starts_with("malformed_marker:"));
    }
}
