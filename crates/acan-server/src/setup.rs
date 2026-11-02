//! The setup page and its routes: the model keys, the role picks, and the status they show.
//!
//! A key goes from the browser to this server once, to be checked with its provider and saved. The
//! response names the outcome, never the key. The body is parsed from its raw bytes, so a malformed
//! request cannot echo its text into an error, and nothing on this path logs the request.

use acan_llm::{KeyOutcome, KeyStatus, ModelClient, Role, SetupError};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
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

/// `GET /api/setup`. Any principal may read it, since it names providers and roles and never a key.
pub(crate) async fn get_status<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
) -> Result<Json<KeyStatus>, ApiError> {
    authenticate(&host, bearer(&headers))?;
    Ok(Json(host.client().key_status()))
}

/// The body of `POST /api/setup/keys`. It has no `Debug` derive, so the key cannot be printed by
/// accident.
#[derive(Deserialize)]
struct KeyBody {
    provider: String,
    key: String,
}

/// `POST /api/setup/keys`. Members only. The key is checked with its provider, and saved if the
/// provider does not refuse it. Saving a key for a provider replaces the key it had.
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
        Err(err) => return Err(setup_error(err)),
    })
}

/// `DELETE /api/setup/keys/{provider}`. Members only. Removes the saved key for the provider, and
/// any role pick that used it. The environment's key, if it has one, still applies.
///
/// Status codes: `200` with `removed` set to whether a saved key was there. `400` for a provider
/// the page does not offer. `409` when a routing file decides the providers.
pub(crate) async fn delete_key<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
    Path(provider): Path<String>,
) -> Result<Response, ApiError> {
    member(&host, &headers)?;
    let removed = host.client().remove_key(&provider).map_err(setup_error)?;
    Ok((
        StatusCode::OK,
        Json(json!({ "provider": provider, "removed": removed })),
    )
        .into_response())
}

/// The body of `PUT /api/setup/roles/{role}`. A missing or `null` provider clears the pick, so the
/// role takes the default again.
#[derive(Deserialize)]
struct RoleBody {
    provider: Option<String>,
}

/// `PUT /api/setup/roles/{role}`. Members only. Picks the provider a role uses, or clears the pick.
///
/// Status codes: `200` with the role and the pick. `400` for an unknown role or provider, and for
/// a role that needs Meta picked for another provider. `409` when a routing file decides, or the
/// provider has no key yet.
pub(crate) async fn put_role<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
    Path(role): Path<String>,
    body: Bytes,
) -> Result<Response, ApiError> {
    member(&host, &headers)?;
    let role: Role = role.parse().map_err(|_| ApiError::NotFound("role"))?;
    let body: RoleBody = serde_json::from_slice(&body)
        .map_err(|_| ApiError::BadRequest("a JSON body with provider"))?;
    host.client()
        .set_role(role, body.provider.as_deref())
        .map_err(setup_error)?;
    Ok((
        StatusCode::OK,
        Json(json!({ "role": role.as_str(), "provider": body.provider })),
    )
        .into_response())
}

/// Maps a setup refusal to the response it gets. The names say what was refused, never a key.
fn setup_error(err: SetupError) -> ApiError {
    match err {
        SetupError::UnknownProvider => ApiError::BadRequest("provider"),
        SetupError::InvalidKey => ApiError::BadRequest("key"),
        SetupError::NeedsMeta => ApiError::BadRequest("needs_meta"),
        SetupError::RoutingFile => ApiError::Conflict("routing_file"),
        SetupError::Unavailable => ApiError::Conflict("unavailable"),
        SetupError::NoKey => ApiError::Conflict("no_key"),
        SetupError::FromEnvironment => ApiError::Conflict("this key comes from the environment"),
        other => ApiError::Internal(other.into()),
    }
}
