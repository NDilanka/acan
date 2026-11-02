//! The setup page and its routes: whether a model key is in use, and saving a key the tester enters.
//!
//! A key goes from the browser to this server once, to be checked with its provider and saved. The
//! response names the outcome, never the key. The body is parsed from its raw bytes, so a malformed
//! request cannot echo its text into an error, and nothing on this path logs the request.

use acan_llm::{KeyOutcome, KeyStatus, ModelClient, PRESETS, SetupError};
use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::host::Host;
use crate::http::{ApiError, authenticate, bearer, member};

/// The page, served as it is. It holds no secret: the owner token comes from the URL fragment or
/// the browser session, and a key is sent only when the tester submits the form.
const PAGE: &str = include_str!("setup/page.html");

/// `GET /setup`: the page. Anyone may load it, since it holds nothing private.
pub(crate) async fn page() -> Html<&'static str> {
    Html(PAGE)
}

/// What `GET /api/setup` returns: the key status, and the providers the page offers.
#[derive(Serialize)]
pub(crate) struct SetupView {
    #[serde(flatten)]
    status: KeyStatus,
    providers: [&'static str; 3],
}

/// `GET /api/setup`. Any principal may read it, since it names a provider and never a key.
pub(crate) async fn get_status<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
) -> Result<Json<SetupView>, ApiError> {
    authenticate(&host, bearer(&headers))?;
    Ok(Json(SetupView {
        status: host.client().key_status(),
        providers: PRESETS,
    }))
}

/// The body of `POST /api/setup/keys`. It has no `Debug` derive, so the key cannot be printed by
/// accident.
#[derive(Deserialize)]
struct KeyBody {
    provider: String,
    key: String,
}

/// `POST /api/setup/keys`. Members only. The key is checked with its provider, and saved if the
/// provider does not refuse it.
///
/// Status codes: `200` stored, with `checked` set when the provider confirmed the key. `400` for an
/// unknown provider, an empty or malformed key, or a body that is not JSON. `409` when a routing
/// file decides the providers, or this server has nowhere to save a key. `422` when the provider
/// refused the key. `503` when the provider could not be reached. Nothing is saved on `422` or
/// `503`.
pub(crate) async fn post_key<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    member(&host, &headers)?;
    let body: KeyBody = serde_json::from_slice(&body)
        .map_err(|_| ApiError::BadRequest("a JSON body with provider and key"))?;
    let provider = body.provider.as_str();
    let outcome = host.client().save_key(provider, body.key.trim()).await;
    Ok(match outcome {
        Ok(KeyOutcome::Stored { checked }) => (
            StatusCode::OK,
            Json(json!({ "result": "stored", "provider": provider, "checked": checked })),
        )
            .into_response(),
        Ok(KeyOutcome::Rejected) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "result": "rejected", "provider": provider })),
        )
            .into_response(),
        Ok(KeyOutcome::Unreachable) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "result": "unreachable", "provider": provider })),
        )
            .into_response(),
        Err(SetupError::UnknownProvider) => {
            return Err(ApiError::BadRequest("provider"));
        }
        Err(SetupError::InvalidKey) => return Err(ApiError::BadRequest("key")),
        Err(SetupError::RoutingFile) => return Err(ApiError::Conflict("routing_file")),
        Err(SetupError::Unavailable) => return Err(ApiError::Conflict("unavailable")),
        Err(other) => return Err(ApiError::Internal(other.into())),
    })
}
