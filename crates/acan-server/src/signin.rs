//! Sign-in links. `acan open` opens the browser on `/#signin=<code>`. The code is a random credential
//! that works once and lasts [`SIGN_IN_TTL`]. The page trades it for the owner's token, so the token
//! itself is never put in a URL.
//!
//! The owner's token is the one in the user's own folder, which a server opened with `acan open`
//! holds. A server started another way has no such folder, so these routes answer `404` there.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use acan_llm::ModelClient;
use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::Principal;
use crate::http::{ApiError, bearer};
use crate::registry::Registry;

/// How long a sign-in code stays valid after it is made.
pub(crate) const SIGN_IN_TTL: Duration = Duration::from_secs(60);

/// How many codes may be held at once. A new code past this evicts the one that expires soonest.
const MAX_CODES: usize = 32;

/// The sign-in codes not yet used, each with the instant it stops being valid.
#[derive(Debug, Default)]
pub(crate) struct SignIns {
    codes: Mutex<HashMap<String, Instant>>,
}

impl SignIns {
    /// A new code, valid from `now` for [`SIGN_IN_TTL`]. Codes that have expired are dropped first.
    pub(crate) fn mint(&self, now: Instant) -> String {
        let code = Uuid::new_v4().simple().to_string();
        let mut codes = self.codes.lock().unwrap_or_else(PoisonError::into_inner);
        codes.retain(|_, expires| *expires > now);
        while codes.len() >= MAX_CODES {
            let soonest = codes
                .iter()
                .min_by_key(|(_, expires)| **expires)
                .map(|(held, _)| held.clone());
            match soonest {
                Some(held) => {
                    codes.remove(&held);
                }
                None => break,
            }
        }
        codes.insert(code.clone(), now + SIGN_IN_TTL);
        code
    }

    /// Uses `code` at `now`. It is true only when the code is held and has not expired, and a code
    /// that is used is gone, so it works once. An unknown code changes nothing.
    pub(crate) fn redeem(&self, code: &str, now: Instant) -> bool {
        let mut codes = self.codes.lock().unwrap_or_else(PoisonError::into_inner);
        codes.retain(|_, expires| *expires > now);
        codes.remove(code).is_some()
    }
}

/// The body of `POST /api/signin/exchange`: the code the link carried.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Exchange {
    code: String,
}

/// `POST /api/signin`. Only the owner may ask for a sign-in code, and the code is then the only
/// thing the link carries.
///
/// Status codes: `200` with `code` and `expires_in` in seconds. `401` for an unknown token. `403`
/// for a token that is not the owner's. `404` with `what: "signin"` when the server holds no
/// user folder.
pub(crate) async fn mint<C: ModelClient + 'static>(
    State(projects): State<Arc<Registry<C>>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let home = projects.home().ok_or(ApiError::NotFound("signin"))?;
    let token = bearer(&headers).ok_or(ApiError::Unauthorized)?;
    match home.tokens.principal(token) {
        Some(Principal::Member(member)) if member == home.owner => {}
        Some(_) => return Err(ApiError::Forbidden("only the owner may sign in")),
        None => return Err(ApiError::Unauthorized),
    }
    let code = projects.signins().mint(Instant::now());
    Ok(Json(json!({
        "code": code,
        "expires_in": SIGN_IN_TTL.as_secs(),
    })))
}

/// `POST /api/signin/exchange`. No token is needed, since the code is the credential. A code works
/// once, within [`SIGN_IN_TTL`] of being made.
///
/// Status codes: `200` with the owner's `token`. `400` for a body that is not an object with a
/// `code`. `401` for a code that is unknown, used or expired. `404` with `what: "signin"` when the
/// server holds no user folder.
pub(crate) async fn exchange<C: ModelClient + 'static>(
    State(projects): State<Arc<Registry<C>>>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let home = projects.home().ok_or(ApiError::NotFound("signin"))?;
    let request: Exchange = serde_json::from_slice(&body)
        .map_err(|_| ApiError::BadRequest("a JSON object with a code"))?;
    if !projects.signins().redeem(&request.code, Instant::now()) {
        return Err(ApiError::Unauthorized);
    }
    let token = home
        .tokens
        .token_of(&Principal::Member(home.owner.clone()))
        .ok_or(ApiError::Unauthorized)?;
    Ok(Json(json!({ "token": token })))
}

#[cfg(test)]
mod tests {
    //! The clock is passed in, so expiry is checked without waiting a minute.

    use super::*;

    #[test]
    fn a_code_works_once_within_its_lifetime() {
        let signins = SignIns::default();
        let now = Instant::now();
        let code = signins.mint(now);
        assert_eq!(code.len(), 32, "a code is 32 hex digits");
        assert!(signins.redeem(&code, now), "the first use works");
        assert!(!signins.redeem(&code, now), "a second use is refused");
    }

    #[test]
    fn a_code_works_just_before_its_expiry_and_not_at_it() {
        let signins = SignIns::default();
        let minted_at = Instant::now();
        let used_in_time = signins.mint(minted_at);
        let used_too_late = signins.mint(minted_at);
        let just_before_expiry = minted_at + SIGN_IN_TTL - Duration::from_millis(1);
        let at_expiry = minted_at + SIGN_IN_TTL;
        assert!(
            signins.redeem(&used_in_time, just_before_expiry),
            "a code used just before its expiry works"
        );
        assert!(
            !signins.redeem(&used_too_late, at_expiry),
            "a code used at its expiry is refused"
        );
    }

    #[test]
    fn an_unknown_code_is_refused_and_changes_nothing() {
        let signins = SignIns::default();
        let now = Instant::now();
        let code = signins.mint(now);
        assert!(!signins.redeem("not-a-code", now));
        assert!(signins.redeem(&code, now), "the real code still works");
    }

    #[test]
    fn the_held_codes_are_capped() {
        let signins = SignIns::default();
        let now = Instant::now();
        let first = signins.mint(now);
        for _ in 0..MAX_CODES {
            signins.mint(now + Duration::from_secs(1));
        }
        assert!(!signins.redeem(&first, now), "the oldest code was evicted");
    }
}
