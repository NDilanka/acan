//! HTTP checks: wait for the app port, send one request, compare status and body.

use std::time::{Duration, Instant};

use acan_core::WitnessOutcome;

use crate::Limits;

/// One readiness attempt against the app port.
const CONNECT_ATTEMPT: Duration = Duration::from_secs(2);
/// Pause between readiness attempts.
const CONNECT_RETRY: Duration = Duration::from_millis(100);

pub(crate) async fn run_http(
    method: &str,
    path: &str,
    expect_status: u16,
    expect_body: Option<&str>,
    limits: &Limits,
) -> WitnessOutcome {
    let Some(base) = limits.http_base_url.as_deref() else {
        return WitnessOutcome::Error {
            detail: "an http check needs a base url in the limits".to_string(),
        };
    };
    let base = match reqwest::Url::parse(base) {
        Ok(base) => base,
        Err(e) => {
            return WitnessOutcome::Error {
                detail: format!("the http base url is invalid: {e}"),
            };
        }
    };
    if reqwest::Url::parse(path).is_ok() {
        return WitnessOutcome::Error {
            detail: "an http check needs a same-origin path, not an absolute URL".to_string(),
        };
    }
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    let url = match crate::record::same_origin_url(&base, &path) {
        Some(url) => url,
        None => {
            return WitnessOutcome::Error {
                detail: "the http path must stay on the configured origin".to_string(),
            };
        }
    };
    let Ok(method) = reqwest::Method::from_bytes(method.as_bytes()) else {
        return WitnessOutcome::Error {
            detail: format!("unknown http method {method:?}"),
        };
    };
    let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
        return WitnessOutcome::Error {
            detail: "the http url has no host or port".to_string(),
        };
    };

    if let Some(detail) = wait_ready(host, port, limits.ready_timeout).await {
        return WitnessOutcome::Error { detail };
    }

    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(limits.request_timeout)
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            return WitnessOutcome::Error {
                detail: format!("could not build an http client: {e}"),
            };
        }
    };
    let response = match client.request(method, url).send().await {
        Ok(response) => response,
        Err(e) => {
            return WitnessOutcome::Error {
                detail: request_error(&e).to_string(),
            };
        }
    };
    let status = response.status().as_u16();
    let body = match read_head(response, limits.tail_bytes).await {
        Ok(body) => body,
        Err(e) => {
            return WitnessOutcome::Error {
                detail: format!("could not read the response body: {e}"),
            };
        }
    };

    if status != expect_status {
        return WitnessOutcome::Fail {
            detail: format!(
                "expected status {expect_status}, got {status}; body (first {} bytes):\n{body}",
                body.len()
            ),
        };
    }
    if let Some(want) = expect_body
        && !body.contains(want)
    {
        return WitnessOutcome::Fail {
            detail: format!(
                "the body did not contain {want:?} (searched the first {} bytes):\n{body}",
                body.len()
            ),
        };
    }
    WitnessOutcome::Pass
}

/// Polls the port until it accepts a connection or the deadline passes. Returns the error
/// detail on timeout.
async fn wait_ready(host: &str, port: u16, ready_timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + ready_timeout;
    loop {
        let attempt = tokio::time::timeout(
            CONNECT_ATTEMPT,
            tokio::net::TcpStream::connect((host, port)),
        )
        .await;
        if matches!(attempt, Ok(Ok(_))) {
            return None;
        }
        if Instant::now() >= deadline {
            return Some(format!(
                "the app did not accept connections on {host}:{port} within {}s",
                ready_timeout.as_secs()
            ));
        }
        tokio::time::sleep(CONNECT_RETRY).await;
    }
}

/// A short label for a failed request. reqwest's own error text echoes the request URL,
/// which could carry credentials from the configured base URL, so details use this instead.
fn request_error(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "the request timed out"
    } else if e.is_connect() {
        "the request could not connect"
    } else {
        "the request failed"
    }
}

/// Reads at most `cap` bytes of the body.
async fn read_head(mut response: reqwest::Response, cap: usize) -> Result<String, reqwest::Error> {
    let mut bytes = Vec::new();
    while bytes.len() < cap {
        let Some(chunk) = response.chunk().await? else {
            break;
        };
        let take = (cap - bytes.len()).min(chunk.len());
        bytes.extend_from_slice(&chunk[..take]);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
