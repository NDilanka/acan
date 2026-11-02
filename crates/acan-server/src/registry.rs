//! The projects one server runs, and the list the UI reads to show them.
//!
//! The registry holds each project's host and nothing else. Which project a request reaches is
//! decided by its path, so a request can only use the host of the project it names.

use std::sync::Arc;

use acan_llm::ModelClient;
use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use serde::Serialize;

use crate::host::Host;
use crate::http::{ApiError, bearer};
use crate::workers::Board;

/// Every project one server runs, and which of them answers the unprefixed routes.
pub(crate) struct Registry<C> {
    default_id: String,
    entries: Vec<Entry<C>>,
}

/// One project as the server runs it.
pub(crate) struct Entry<C> {
    /// The URL segment and the key every API and log line uses for this project.
    pub(crate) id: String,
    /// The name the UI shows.
    pub(crate) name: String,
    /// The project's host: its writer, its snapshot, its tokens and its git state.
    pub(crate) host: Host<C>,
    /// The state of this project's local workers, which `GET /api/p/{id}/workers` reports.
    pub(crate) board: Board,
}

impl<C> Registry<C> {
    /// A registry of `entries`, whose default is `default_id`.
    pub(crate) fn new(default_id: String, entries: Vec<Entry<C>>) -> Self {
        Self {
            default_id,
            entries,
        }
    }

    /// The id of the project the unprefixed routes map to.
    pub(crate) fn default_id(&self) -> &str {
        &self.default_id
    }

    /// Every project, in the order the server was given them.
    pub(crate) fn entries(&self) -> &[Entry<C>] {
        &self.entries
    }
}

/// One project as `GET /api/projects` lists it.
#[derive(Debug, Serialize)]
pub(crate) struct ProjectView {
    id: String,
    name: String,
    default: bool,
    status: &'static str,
    protected_branch: Option<String>,
    machines: usize,
}

/// The projects the caller's token is a member of. A member is a principal listed in that
/// project's tokens file, so a token sees only the projects it may use. A token that is a member
/// of none gets 401, the same answer an unknown token gets from any route.
pub(crate) async fn list_projects<C: ModelClient + 'static>(
    State(projects): State<Arc<Registry<C>>>,
    headers: HeaderMap,
) -> Result<Json<Vec<ProjectView>>, ApiError> {
    let token = bearer(&headers).ok_or(ApiError::Unauthorized)?;
    let views: Vec<ProjectView> = projects
        .entries()
        .iter()
        .filter(|entry| entry.host.tokens().principal(token).is_some())
        .map(|entry| ProjectView {
            id: entry.id.clone(),
            name: entry.name.clone(),
            default: entry.id == projects.default_id(),
            status: "ready",
            protected_branch: entry
                .host
                .workspace()
                .map(|workspace| workspace.repo().protected_branch().to_owned()),
            machines: entry.host.scan().plant.machines.len(),
        })
        .collect();
    if views.is_empty() {
        return Err(ApiError::Unauthorized);
    }
    Ok(Json(views))
}
