//! Turns driver replies and capture sources into the records `acan_intent` admits.
//!
//! Every function here is pure. A header or cookie capture is a string. A body capture reads
//! through the same [`ObservationPath`] lookup compare uses, and a capture's fingerprint is the
//! hash of exactly that value, so the compare module reads back what the runner recorded.

use std::collections::BTreeMap;
use std::str::FromStr;

use acan_intent::{
    CaptureKind, CaptureName, CaptureSource, Channels, Evidence, HttpMethod, IncompleteReason,
    MissingCapture, ObservationPath, RouteEntry, RouteTemplate,
};
use serde_json::{Map, Value};

use crate::driver::Reply;

/// Most redirects the runner follows for one step.
pub(crate) const MAX_REDIRECTS: u8 = 5;

/// Response headers that never reach the header channel. `date` changes between runs of the same
/// commit. `set-cookie` is recorded as cookies, and its value is a fresh session token that no
/// normalization projection of the probe masks.
const UNRECORDED_HEADERS: [&str; 2] = ["date", "set-cookie"];

/// The channels one response produced, and why the observation is incomplete, if it is.
///
/// A body that is not JSON is kept as an opaque value. A body whose content type claims JSON and
/// does not parse marks the observation incomplete, and so does a body the output bound cut. A
/// binary body has no bytes on the wire to hash, so it marks the observation incomplete too.
pub(crate) fn channels(reply: &Reply) -> (Channels, Vec<IncompleteReason>) {
    let mut channels = Channels {
        status: reply.status,
        ..Channels::default()
    };
    for (name, value) in &reply.headers {
        if UNRECORDED_HEADERS.contains(&name.as_str()) {
            continue;
        }
        if let Some(text) = header_text(value) {
            channels.headers.insert(name.clone(), text);
        }
    }
    if let Some(set_cookie) = reply.headers.get("set-cookie") {
        for line in header_lines(set_cookie) {
            if let Some((name, value)) = parse_cookie(line) {
                channels.cookies.insert(name, value);
            }
        }
    }
    let mut reasons = Vec::new();
    if reply.truncated {
        reasons.push(IncompleteReason::OutputTruncated {});
    } else if reply.binary {
        reasons.push(IncompleteReason::BodyNotJson {});
    } else if let Some(text) = reply.body.as_deref().filter(|text| !text.is_empty()) {
        let content_type = header(reply, "content-type");
        if let Err(reason) = channels.set_body_text(text, content_type.as_deref()) {
            reasons.push(reason);
        }
    }
    (channels, reasons)
}

/// One response header by lowercase name, joined the way a comma-separated header is.
pub(crate) fn header(reply: &Reply, name: &str) -> Option<String> {
    reply.headers.get(name).and_then(header_text)
}

fn header_lines(value: &Value) -> Vec<&str> {
    match value {
        Value::String(text) => vec![text.as_str()],
        Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

fn header_text(value: &Value) -> Option<String> {
    let lines = header_lines(value);
    (!lines.is_empty()).then(|| lines.join(", "))
}

/// Parses the `name=value` pair at the front of one `Set-Cookie` line.
pub(crate) fn parse_cookie(line: &str) -> Option<(String, String)> {
    let pair = line.split(';').next()?;
    let (name, value) = pair.split_once('=')?;
    let name = name.trim();
    (!name.is_empty()).then(|| (name.to_owned(), value.trim().to_owned()))
}

/// The route-table pattern a request path matches.
///
/// Among the table's entries of the same method that match, the one with the fewest parameters
/// wins, so a literal route such as `/dashboard` is not labelled `/:code`. A query string does
/// not take part in matching. `None` when the table has no match.
pub(crate) fn route_template(
    method: HttpMethod,
    path: &str,
    routes: &[RouteEntry],
) -> Option<RouteTemplate> {
    let bare = path.split('?').next().unwrap_or(path);
    let segments: Vec<&str> = bare.split('/').collect();
    routes
        .iter()
        .filter(|route| {
            route.method == method && pattern_matches(route.route_template.as_str(), &segments)
        })
        .min_by_key(|route| {
            route
                .route_template
                .as_str()
                .split('/')
                .filter(|part| part.starts_with(':'))
                .count()
        })
        .map(|route| route.route_template.clone())
}

fn pattern_matches(pattern: &str, segments: &[&str]) -> bool {
    let parts: Vec<&str> = pattern.split('/').collect();
    parts.len() == segments.len()
        && parts
            .iter()
            .zip(segments)
            .all(|(part, segment)| match part.strip_prefix(':') {
                Some(_) => !segment.is_empty(),
                None => part == segment,
            })
}

/// Fills the `${name}` placeholders of `text` with the bound captures' text. A string is used
/// as it is; any other JSON value is used as its JSON text.
///
/// # Errors
///
/// Returns the first placeholder whose capture is not bound.
pub(crate) fn fill(
    text: &str,
    values: &BTreeMap<CaptureName, Value>,
) -> Result<String, CaptureName> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find("${") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let Some(close) = after.find('}') else {
            out.push_str(&rest[open..]);
            return Ok(out);
        };
        let name = &after[..close];
        match CaptureName::from_str(name) {
            Ok(binding) => match values.get(&binding) {
                Some(Value::String(bound)) => out.push_str(bound),
                Some(other) => out.push_str(&other.to_string()),
                None => return Err(binding),
            },
            // Templates are validated when they are decoded, so this placeholder is literal text.
            Err(_) => out.push_str(&rest[open..open + 3 + close]),
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// [`fill`] applied to every string leaf of a JSON value. Keys are left as they are.
///
/// # Errors
///
/// Returns the first placeholder whose capture is not bound.
pub(crate) fn fill_json(
    value: &Value,
    values: &BTreeMap<CaptureName, Value>,
) -> Result<Value, CaptureName> {
    Ok(match value {
        Value::String(text) => Value::String(fill(text, values)?),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| fill_json(item, values))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, field)| -> Result<(String, Value), CaptureName> {
                    Ok((key.clone(), fill_json(field, values)?))
                })
                .collect::<Result<Map<String, Value>, CaptureName>>()?,
        ),
        other => other.clone(),
    })
}

/// Reads a capture's value from the channels of its source step.
///
/// Header and cookie values are strings. A body value is what [`Channels::lookup`] holds at the
/// field path, exactly as compare reads it back. A session or identifier capture must hold a
/// string, so any other JSON value is malformed.
///
/// # Errors
///
/// Returns [`MissingCapture::Absent`] when the source holds nothing at the path and
/// [`MissingCapture::Malformed`] when the value has the wrong type.
pub(crate) fn read_capture(
    channels: &Channels,
    source: &CaptureSource,
    kind: CaptureKind,
) -> Result<Value, MissingCapture> {
    let value = match source {
        CaptureSource::Header { name } => channels
            .headers
            .get(name)
            .map(|text| Value::String(text.clone())),
        CaptureSource::Cookie { name } => channels
            .cookies
            .get(name)
            .map(|text| Value::String(text.clone())),
        CaptureSource::Body { path } => {
            let separator = if path.as_str().starts_with('[') {
                ""
            } else {
                "."
            };
            let Ok(parsed) =
                ObservationPath::from_str(&format!("body{separator}{}", path.as_str()))
            else {
                return Err(MissingCapture::Malformed);
            };
            match channels.lookup(&parsed) {
                Evidence::Present { value } => Some(value),
                Evidence::Missing {} => None,
            }
        }
    };
    let value = value.ok_or(MissingCapture::Absent)?;
    let identity = matches!(kind, CaptureKind::Session | CaptureKind::Identifier);
    if identity && !value.is_string() {
        return Err(MissingCapture::Malformed);
    }
    Ok(value)
}

/// The path a `Location` header sends the client to, when it stays on the app's own origin.
pub(crate) fn same_origin_path(location: &str, port: u16) -> Option<String> {
    if !location.starts_with('/') && !location.starts_with("http://") {
        return None;
    }
    let base = reqwest::Url::parse(&format!("http://127.0.0.1:{port}")).ok()?;
    let url = same_origin_url(&base, location)?;
    let mut path = url.path().to_owned();
    if let Some(query) = url.query() {
        path.push('?');
        path.push_str(query);
    }
    Some(path)
}

/// Resolves a URL without permitting authority, credential or scheme changes.
pub(crate) fn same_origin_url(base: &reqwest::Url, location: &str) -> Option<reqwest::Url> {
    if location.starts_with("//")
        || location.contains('\\')
        || location
            .chars()
            .any(|c| c.is_control() || c.is_whitespace())
        || !matches!(base.scheme(), "http" | "https")
        || !base.username().is_empty()
        || base.password().is_some()
    {
        return None;
    }
    let url = base.join(location).ok()?;
    (url.scheme() == base.scheme()
        && url.host_str() == base.host_str()
        && url.port_or_known_default() == base.port_or_known_default()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

/// The method as it appears on the request line.
pub(crate) fn method_text(method: HttpMethod) -> &'static str {
    match method {
        HttpMethod::Get => "GET",
        HttpMethod::Head => "HEAD",
        HttpMethod::Post => "POST",
        HttpMethod::Put => "PUT",
        HttpMethod::Patch => "PATCH",
        HttpMethod::Delete => "DELETE",
        HttpMethod::Options => "OPTIONS",
    }
}

#[cfg(test)]
mod tests {
    use acan_intent::ContentHash;

    use super::*;

    fn binding(name: &str) -> CaptureName {
        CaptureName::from_str(name).expect("a valid capture name")
    }

    #[test]
    fn a_placeholder_fills_from_a_bound_string_and_refuses_an_unbound_one() {
        let mut values = BTreeMap::new();
        values.insert(binding("alice_session"), Value::String("tok".to_owned()));
        assert_eq!(
            fill("Bearer ${alice_session}!", &values).expect("bound"),
            "Bearer tok!"
        );
        assert_eq!(
            fill("/api/links/${link_code}", &values),
            Err(binding("link_code"))
        );
    }

    #[test]
    fn an_open_placeholder_stays_literal() {
        let values = BTreeMap::new();
        assert_eq!(fill("a ${ b", &values).expect("literal"), "a ${ b");
    }

    #[test]
    fn a_cookie_line_yields_its_name_and_value() {
        assert_eq!(
            parse_cookie("session=abc; HttpOnly; Path=/"),
            Some(("session".to_owned(), "abc".to_owned()))
        );
        assert_eq!(parse_cookie("=nope"), None);
    }

    #[test]
    fn the_fewest_parameter_route_matches() {
        let routes: Vec<RouteEntry> = serde_json::from_value(serde_json::json!([
            { "method": "GET", "route_template": "/:code", "route_path": { "machines": ["links"], "source": "static" }, "status": { "status": "covered" } },
            { "method": "GET", "route_template": "/dashboard", "route_path": { "machines": ["dashboard"], "source": "static" }, "status": { "status": "covered" } }
        ]))
        .expect("route entries decode");
        let dashboard = route_template(HttpMethod::Get, "/dashboard?x=1", &routes).expect("match");
        assert_eq!(dashboard.as_str(), "/dashboard");
        let code = route_template(HttpMethod::Get, "/abc", &routes).expect("match");
        assert_eq!(code.as_str(), "/:code");
        assert!(route_template(HttpMethod::Post, "/abc", &routes).is_none());
    }

    #[test]
    fn same_origin_locations_are_followed_and_others_are_not() {
        assert_eq!(same_origin_path("/x?y=1", 3000).as_deref(), Some("/x?y=1"));
        assert_eq!(
            same_origin_path("http://127.0.0.1:3000/z", 3000).as_deref(),
            Some("/z")
        );
        assert_eq!(same_origin_path("http://example.com/z", 3000), None);
        assert_eq!(same_origin_path("//example.com/z", 3000), None);
        for path in [
            "/\\evil/x",
            "\\\\evil/x",
            "http://evil@127.0.0.1:3000/x",
            "/\n/evil",
        ] {
            assert_eq!(same_origin_path(path, 3000), None);
        }
    }

    fn reply(content_type: Option<&str>, body: &str) -> Reply {
        let headers = match content_type {
            Some(value) => {
                BTreeMap::from([("content-type".to_owned(), Value::String(value.to_owned()))])
            }
            None => BTreeMap::new(),
        };
        Reply {
            ok: true,
            crashed: false,
            detail: String::new(),
            status: Some(200),
            headers,
            body: Some(body.to_owned()),
            binary: false,
            truncated: false,
        }
    }

    #[test]
    fn a_body_that_is_not_json_is_kept_opaque_and_the_observation_stays_complete() {
        let html = "<!doctype html><h1>Links</h1>";
        let (channels, reasons) = channels(&reply(Some("text/html; charset=utf-8"), html));
        assert!(reasons.is_empty(), "{reasons:?}");
        let body = channels.body.expect("the body is kept");
        assert_eq!(
            body["acan_opaque_body"]["content_type"],
            "text/html; charset=utf-8"
        );
        assert_eq!(body["acan_opaque_body"]["bytes"], html.len());
        assert_eq!(
            body["acan_opaque_body"]["sha256"],
            ContentHash::of_bytes(html.as_bytes()).to_string()
        );
    }

    #[test]
    fn a_body_that_claims_json_and_does_not_parse_is_incomplete() {
        let (channels, reasons) = channels(&reply(Some("application/json"), "<html>"));
        assert_eq!(reasons, vec![IncompleteReason::BodyNotJson {}]);
        assert!(channels.body.is_none());
    }

    #[test]
    fn a_json_body_under_the_opaque_key_is_kept_opaque() {
        let text = r#"{"acan_opaque_body": {"bytes": 1}}"#;
        let (channels, reasons) = channels(&reply(Some("application/json"), text));
        assert!(reasons.is_empty(), "{reasons:?}");
        let body = channels.body.expect("the body is kept");
        assert_eq!(body["acan_opaque_body"]["content_type"], "application/json");
        assert_eq!(body["acan_opaque_body"]["bytes"], text.len());
    }
}
