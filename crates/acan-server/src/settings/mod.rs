//! The server's settings: the harness, the projects, the budgets, the look and the notifications.
//!
//! `GET /api/settings` reads the settings. Any member may read them. `PUT /api/settings` changes
//! them, and only the owner may do that. A change is checked as a whole before anything is written,
//! so a change that fails leaves every setting as it was. See [`validate`] for the rules.

pub(crate) mod data;
pub(crate) mod file;
mod update;
mod validate;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use acan_core::{AgentId, MemberId};
use acan_llm::{ModelClient, Role};
use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::STATE_DIR;
use crate::add;
use crate::auth::{Principal, Tokens};
use crate::http::{ApiError, bearer};
use crate::projects::same_folder;
use crate::registry::Registry;
use crate::release::{self, VersionInfo};

pub(crate) use file::load;
use validate::Patch;

/// The file the settings are kept in.
pub(crate) const FILE: &str = "settings.json";

/// The version of the settings document `GET /api/settings` returns.
const DOCUMENT_VERSION: u32 = 1;

/// Why an agent may not read the settings.
const ONLY_MEMBERS_READ: &str = "only members may read settings";

/// Why anyone but the owner may not change the settings.
const ONLY_OWNER_CHANGES: &str = "only the owner may change settings";

/// Why anyone but the owner may not read or rotate a token.
const ONLY_OWNER_TOKENS: &str = "only the owner may manage tokens";

/// Why anyone but the owner may not check for or install an update.
const ONLY_OWNER_UPDATES: &str = "only the owner may update Acan";

/// Defines a setting whose values have fixed names in the API and in the settings file. The names
/// are the ones the settings page sends, so `parse` reads what the page sent.
macro_rules! named {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal,)+ }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        pub(crate) enum $name {
            $(
                #[serde(rename = $text)]
                $variant,
            )+
        }

        impl $name {
            /// The names the value may have, in the order the settings page lists them.
            pub(crate) const NAMES: &'static [&'static str] = &[$($text),+];

            /// The value `name` names, or `None` when no value has that name.
            pub(crate) fn parse(name: &str) -> Option<Self> {
                match name {
                    $($text => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

named! {
    /// The adapter a role runs its calls through. `builtin` is the default.
    Adapter {
        Builtin => "builtin",
        ClaudeCode => "claude_code",
        Codex => "codex",
        Cursor => "cursor",
        Grok => "grok",
        Opencode => "opencode",
        Antigravity => "antigravity",
        Fx => "fx",
        Custom => "custom",
    }
}

named! {
    /// The colour theme of the page.
    Theme {
        Paper => "paper",
        Dark => "dark",
        Blueprint => "blueprint",
        HighContrast => "high_contrast",
    }
}

named! {
    /// How many labels the plant view shows.
    LabelDensity {
        Sparse => "sparse",
        Normal => "normal",
        Dense => "dense",
    }
}

/// What the settings file holds.
/// A missing section, or field, reads as its default. A key this version does not use is kept in
/// an `unknown` map at each level and written back on save, so a file another build wrote still
/// loads, and its keys are not lost. See [`Stored::unknown_keys`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Stored {
    pub(crate) harness: Harness,
    pub(crate) projects: Projects,
    pub(crate) budgets: Budgets,
    pub(crate) look: Look,
    pub(crate) notifications: Notifications,
    /// Top-level keys this version does not use, with their values as the file holds them.
    #[serde(flatten)]
    pub(crate) unknown: BTreeMap<String, Value>,
}

impl Stored {
    /// The keys of the settings file this version does not use, as dotted paths such as
    /// `projects.live`. Only the names are returned, never the values.
    pub(crate) fn unknown_keys(&self) -> Vec<String> {
        let mut keys = named("", &self.unknown);
        keys.extend(named("harness.", &self.harness.unknown));
        keys.extend(named("projects.", &self.projects.unknown));
        keys.extend(named("budgets.", &self.budgets.unknown));
        keys.extend(named("look.", &self.look.unknown));
        keys.extend(named("notifications.", &self.notifications.unknown));
        keys
    }

    /// These settings with every key this version does not use left out, at each level that
    /// [`Stored::unknown_keys`] reads. A response shows these, so it names only the settings the
    /// server takes, and a page that sends a section back as it read it is not refused for a key
    /// the API does not take.
    fn without_unknown(&self) -> Self {
        let mut known = self.clone();
        known.unknown.clear();
        known.harness.unknown.clear();
        known.projects.unknown.clear();
        known.budgets.unknown.clear();
        known.look.unknown.clear();
        known.notifications.unknown.clear();
        known
    }
}

/// The keys of `unknown`, each with `prefix` in front.
fn named(prefix: &str, unknown: &BTreeMap<String, Value>) -> Vec<String> {
    unknown.keys().map(|key| format!("{prefix}{key}")).collect()
}

/// Logs one warning when the settings the server started with hold keys it does not use. The
/// warning names the keys and no values. The keys are kept in the file and ignored.
pub(crate) fn warn_unknown_keys(stored: &Stored) {
    let keys = stored.unknown_keys();
    if !keys.is_empty() {
        tracing::warn!(
            keys = %keys.join(", "),
            "the settings file has keys this version does not use; they are kept and ignored"
        );
    }
}

/// The adapter each role runs through, and the command the `custom` adapter runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Harness {
    /// The adapter of each role that has one. A role with no entry runs the `builtin` adapter.
    pub(crate) roles: BTreeMap<Role, Adapter>,
    /// The command the `custom` adapter runs, one argument per entry. Empty when none is set.
    pub(crate) custom_command: Vec<String>,
    /// Keys this version does not use, kept as the file holds them. See [`Stored`].
    #[serde(flatten)]
    pub(crate) unknown: BTreeMap<String, Value>,
}

/// The projects a change records. Adds and removals take effect at the next start, and stay in
/// effect until a later change undoes them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Projects {
    /// Folders served as projects beyond the ones the command line or projects file names, as
    /// absolute paths.
    pub(crate) add: Vec<String>,
    /// Folders of projects that are not served, as absolute paths. A folder stays here until a
    /// change adds it again.
    pub(crate) remove: Vec<String>,
    /// Keys this version does not use, kept as the file holds them. See [`Stored`].
    #[serde(flatten)]
    pub(crate) unknown: BTreeMap<String, Value>,
}

/// The limits the budgets set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Budgets {
    /// The repair rounds an order gets. Matches the budget acan-core offers before the first
    /// confirmation of an order.
    pub(crate) repair_rounds: u32,
    /// The new episodes an order may open.
    pub(crate) max_episodes: u32,
    /// The wall time a repair may take, in milliseconds.
    pub(crate) wall_time_ms: u64,
    /// How long a task may declare with no progress before it is stuck, in milliseconds.
    pub(crate) stuck_declare_ms: u64,
    /// How long a build may run with no progress before it is stuck, in milliseconds.
    pub(crate) stuck_build_ms: u64,
    /// How far a plant may drift before it is flagged, in percent.
    pub(crate) drift_threshold: u32,
    /// Keys this version does not use, kept as the file holds them. See [`Stored`].
    #[serde(flatten)]
    pub(crate) unknown: BTreeMap<String, Value>,
}

impl Default for Budgets {
    fn default() -> Self {
        Self {
            repair_rounds: 2,
            max_episodes: 4,
            wall_time_ms: 600_000,
            stuck_declare_ms: 300_000,
            stuck_build_ms: 900_000,
            drift_threshold: 50,
            unknown: BTreeMap::new(),
        }
    }
}

/// How the page looks. The server stores these and the page applies them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Look {
    pub(crate) theme: Theme,
    /// Whether the page opens top-down. The isometric view is the default.
    pub(crate) top_down_default: bool,
    pub(crate) motion: bool,
    pub(crate) idle_bob: bool,
    pub(crate) reduced_motion: bool,
    pub(crate) label_density: LabelDensity,
    /// Keys this version does not use, kept as the file holds them. See [`Stored`].
    #[serde(flatten)]
    pub(crate) unknown: BTreeMap<String, Value>,
}

impl Default for Look {
    fn default() -> Self {
        Self {
            theme: Theme::Paper,
            top_down_default: false,
            motion: true,
            idle_bob: true,
            reduced_motion: false,
            label_density: LabelDensity::Normal,
            unknown: BTreeMap::new(),
        }
    }
}

/// What the page notifies the tester about. The server stores these and the page applies them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Notifications {
    pub(crate) needs_you: bool,
    pub(crate) sound: bool,
    pub(crate) browser: bool,
    /// Keys this version does not use, kept as the file holds them. See [`Stored`].
    #[serde(flatten)]
    pub(crate) unknown: BTreeMap<String, Value>,
}

impl Default for Notifications {
    fn default() -> Self {
        Self {
            needs_you: true,
            sound: false,
            browser: false,
            unknown: BTreeMap::new(),
        }
    }
}

/// A part of the settings that a change names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Section {
    Harness,
    Projects,
    Budgets,
    Look,
    Notifications,
}

/// How the setting at `path` takes effect, as the `applied` map of a response and `GET` name it.
/// The path is dotted, as a request names the setting. `stored` means it is saved, and no server
/// path reads it yet. `restart` means it takes effect at the next start. `client` means the page
/// applies it.
fn applied_label(path: &str) -> &'static str {
    match path.split('.').next() {
        Some("projects") => "restart",
        Some("look" | "notifications") => "client",
        _ => "stored",
    }
}

/// Records the setting at `path`, and how it takes effect.
fn note(applied: &mut BTreeMap<String, &'static str>, path: String) {
    let label = applied_label(&path);
    applied.insert(path, label);
}

/// Records each setting under `value`, at its dotted `path`. An object's fields are walked.
/// Any other value, arrays included, is one setting.
fn note_each(value: &Value, path: &str, applied: &mut BTreeMap<String, &'static str>) {
    match value {
        Value::Object(fields) => {
            for (name, field) in fields {
                let child = if path.is_empty() {
                    name.clone()
                } else {
                    format!("{path}.{name}")
                };
                note_each(field, &child, applied);
            }
        }
        _ => note(applied, path.to_owned()),
    }
}

/// The settings the server holds, and what the routes read and change them through.
pub(crate) struct Settings<C> {
    /// The projects. Every project's tokens may authenticate a request.
    projects: Arc<Registry<C>>,
    /// The member the owner token belongs to. Only this member may change the settings.
    founder: MemberId,
    /// The settings file.
    file: PathBuf,
    /// The settings as the server last loaded or saved them. Replaced only after a change is
    /// written.
    stored: RwLock<Stored>,
    /// Held for a whole change, so two changes never interleave.
    changes: Mutex<()>,
}

/// Who a request acts for, as the settings see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Caller {
    Owner,
    Member,
}

impl<C: ModelClient + 'static> Settings<C> {
    /// The settings `stored`, kept in `file`, for the projects in `projects`. `founder` is the
    /// member whose token is the owner's.
    pub(crate) fn new(
        file: PathBuf,
        stored: Stored,
        projects: Arc<Registry<C>>,
        founder: MemberId,
    ) -> Self {
        Self {
            projects,
            founder,
            file,
            stored: RwLock::new(stored),
            changes: Mutex::new(()),
        }
    }

    /// The principal the request's bearer token acts for. Each project keeps its own tokens, and the
    /// user's folder keeps the owner's, so both are asked.
    fn principal(&self, headers: &HeaderMap) -> Result<Principal, ApiError> {
        let token = bearer(headers).ok_or(ApiError::Unauthorized)?;
        self.projects.principal(token).ok_or(ApiError::Unauthorized)
    }

    /// Who the request acts for. An agent is refused with `why`.
    fn caller(&self, headers: &HeaderMap, why: &'static str) -> Result<Caller, ApiError> {
        match self.principal(headers)? {
            Principal::Member(member) if member == self.founder => Ok(Caller::Owner),
            Principal::Member(_) => Ok(Caller::Member),
            Principal::Agent(_) => Err(ApiError::Forbidden(why)),
        }
    }

    /// The settings as the server holds them now.
    fn stored(&self) -> Stored {
        self.stored
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The document `GET /api/settings` returns to `caller`. It holds only settings this version
    /// uses.
    fn view(&self, caller: Caller, stored: &Stored) -> View {
        let stored = &stored.without_unknown();
        View {
            version: DOCUMENT_VERSION,
            you: match caller {
                Caller::Owner => "owner",
                Caller::Member => "member",
            },
            editable: caller == Caller::Owner,
            harness: HarnessView::of(&stored.harness),
            projects: self.projects_view(),
            budgets: stored.budgets.clone(),
            look: stored.look.clone(),
            notifications: stored.notifications.clone(),
            applied: Self::applied_fields(),
            project_changes: (caller == Caller::Owner).then(|| ProjectChangesView {
                add: self.pending_adds(stored),
                remove: stored.projects.remove.clone(),
            }),
            tokens: (caller == Caller::Owner).then(|| self.tokens_view()),
            about: release::version_info(),
        }
    }

    /// The folders the next start adds: each folder in the settings that no running project serves.
    /// A folder a live add opened is served, so it is not pending. It stays in the settings, so the
    /// next start serves it too.
    fn pending_adds(&self, stored: &Stored) -> Vec<String> {
        let served: Vec<PathBuf> = self
            .projects
            .entries()
            .iter()
            .map(|entry| entry.host.repo().to_path_buf())
            .collect();
        stored
            .projects
            .add
            .iter()
            .filter(|folder| {
                !served
                    .iter()
                    .any(|repo| same_folder(repo, Path::new(folder)))
            })
            .cloned()
            .collect()
    }

    /// How each setting a change can name takes effect, by its dotted path.
    fn applied_fields() -> BTreeMap<String, &'static str> {
        let mut applied = BTreeMap::new();
        note_each(
            &json!({
                "budgets": Budgets::default(),
                "look": Look::default(),
                "notifications": Notifications::default(),
            }),
            "",
            &mut applied,
        );
        for role in Role::ALL {
            note(&mut applied, format!("harness.roles.{role}"));
        }
        note(&mut applied, "harness.custom_command".to_owned());
        note(&mut applied, "projects.add".to_owned());
        note(&mut applied, "projects.remove".to_owned());
        applied
    }

    /// Each running project, by id and name.
    fn projects_view(&self) -> Vec<ProjectView> {
        self.projects
            .entries()
            .iter()
            .map(|entry| ProjectView {
                id: entry.id.clone(),
                name: entry.name.clone(),
            })
            .collect()
    }

    /// Refuses any caller but the owner with `why`. An unknown token is refused as unauthorized.
    fn owner_only(&self, headers: &HeaderMap, why: &'static str) -> Result<(), ApiError> {
        match self.caller(headers, why)? {
            Caller::Owner => Ok(()),
            Caller::Member => Err(ApiError::Forbidden(why)),
        }
    }

    /// The token `principal` holds in the first project that holds one. Each project keeps its own
    /// tokens, and the owner's and the agents' tokens are the same text in every project they act
    /// in, so the first one is the token.
    fn token_held_by(&self, principal: &Principal) -> Option<String> {
        self.projects
            .entries()
            .iter()
            .find_map(|entry| entry.host.tokens().token_of(principal))
            .or_else(|| self.projects.home()?.tokens.token_of(principal))
    }

    /// The owner's view of the tokens: the owner's token and each agent's, all masked.
    fn tokens_view(&self) -> TokensView {
        let owner = self
            .token_held_by(&Principal::Member(self.founder.clone()))
            .map(|token| mask(&token));
        let agents: BTreeSet<AgentId> = self
            .projects
            .entries()
            .iter()
            .flat_map(|entry| entry.host.tokens().agents())
            .collect();
        let agents = agents
            .into_iter()
            .filter_map(|agent| {
                let token = self.token_held_by(&Principal::Agent(agent.clone()))?;
                Some(AgentTokenView {
                    id: agent.as_str().to_owned(),
                    masked: mask(&token),
                })
            })
            .collect();
        TokensView { owner, agents }
    }
}

/// Masks a token for display: its first five characters, an ellipsis, and its last four. A token
/// too short to show that much is hidden entirely, so a masked token never shows most of a token.
fn mask(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() < MASK_MIN_LEN {
        return "…".to_owned();
    }
    let head: String = chars[..MASK_HEAD].iter().collect();
    let tail: String = chars[chars.len() - MASK_TAIL..].iter().collect();
    format!("{head}…{tail}")
}

/// The shortest token that is masked with its ends shown. Shorter tokens are hidden entirely.
const MASK_MIN_LEN: usize = 12;
/// The characters a masked token shows from its start.
const MASK_HEAD: usize = 5;
/// The characters a masked token shows from its end.
const MASK_TAIL: usize = 4;

/// The document `GET /api/settings` returns. Its field names are the contract the page reads.
#[derive(Debug, Serialize)]
struct View {
    version: u32,
    you: &'static str,
    editable: bool,
    harness: HarnessView,
    projects: Vec<ProjectView>,
    budgets: Budgets,
    look: Look,
    notifications: Notifications,
    /// How each setting takes effect, by its dotted path: `stored`, `restart` or `client`.
    applied: BTreeMap<String, &'static str>,
    /// Only the owner's document has this key. The folders settings added and removed as projects,
    /// which the next start applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    project_changes: Option<ProjectChangesView>,
    /// Only the owner's document has this key. Its contents are filled in by the token routes.
    #[serde(skip_serializing_if = "Option::is_none")]
    tokens: Option<TokensView>,
    about: VersionInfo,
}

/// The folders settings added and removed as projects. They are paths, so only the owner sees
/// them.
#[derive(Debug, Serialize)]
struct ProjectChangesView {
    add: Vec<String>,
    remove: Vec<String>,
}

/// The owner's view of the tokens. Every token is masked, so no response carries a whole one.
#[derive(Debug, Serialize)]
struct TokensView {
    /// The owner's token, masked. `None` when no project holds one.
    owner: Option<String>,
    /// Each agent's token, masked, by agent id, in id order.
    agents: Vec<AgentTokenView>,
}

/// One agent's token as the owner sees it.
#[derive(Debug, Serialize)]
struct AgentTokenView {
    id: String,
    masked: String,
}

/// The adapter of every role, and the custom command.
#[derive(Debug, Serialize)]
struct HarnessView {
    roles: BTreeMap<&'static str, Adapter>,
    custom_command: Vec<String>,
}

impl HarnessView {
    fn of(harness: &Harness) -> Self {
        Self {
            roles: Role::ALL
                .iter()
                .map(|role| {
                    (
                        role.as_str(),
                        harness.roles.get(role).copied().unwrap_or(Adapter::Builtin),
                    )
                })
                .collect(),
            custom_command: harness.custom_command.clone(),
        }
    }
}

/// One project as the settings list it.
#[derive(Debug, Serialize)]
struct ProjectView {
    id: String,
    name: String,
}

/// `GET /api/settings`. Any member may read the settings. An agent gets 403, and an unknown token
/// gets 401. Only the owner is told the settings are editable.
async fn get_settings<C: ModelClient + 'static>(
    State(state): State<Arc<Settings<C>>>,
    headers: HeaderMap,
) -> Result<Json<View>, ApiError> {
    let caller = state.caller(&headers, ONLY_MEMBERS_READ)?;
    Ok(Json(state.view(caller, &state.stored())))
}

/// `PUT /api/settings`. Only the owner may change the settings. The body names the sections to
/// change and the values in each. The whole change is checked before anything is written, and a
/// failing field changes nothing.
///
/// Status codes: `200` with the settings after the change, how each changed setting takes effect
/// by its dotted path. `400` for a body that is not a settings object or names a section or field
/// the server does not take. `401` for an unknown token. `403` for any token but the owner's. `422`
/// with each failing field. `500` when the settings file cannot be written.
async fn put_settings<C: ModelClient + 'static>(
    State(state): State<Arc<Settings<C>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    if state.caller(&headers, ONLY_OWNER_CHANGES)? != Caller::Owner {
        return Err(ApiError::Forbidden(ONLY_OWNER_CHANGES));
    }
    let raw: Value = serde_json::from_slice(&body)
        .map_err(|_| ApiError::BadRequest("a JSON object of settings sections"))?;
    let patch = Patch::deserialize(&raw)
        .map_err(|_| ApiError::BadRequest("a JSON object of settings sections"))?;
    let _turn = state.changes.lock().await;
    let entries = state.projects.entries();
    let running: Vec<(&str, PathBuf)> = entries
        .iter()
        .map(|entry| (entry.id.as_str(), entry.host.repo().to_path_buf()))
        .collect();
    let default_id = state.projects.default_id();
    let changes = validate::check(&patch, &state.stored(), &running, &default_id)
        .map_err(ApiError::Invalid)?;

    if !changes.sections.is_empty() {
        file::save(&state.file, &changes.next).map_err(|err| ApiError::Internal(err.into()))?;
        *state.stored.write().unwrap_or_else(PoisonError::into_inner) = changes.next.clone();
    }

    let mut applied = BTreeMap::new();
    if let Value::Object(sections) = &raw {
        for (name, section) in sections.iter().filter(|(_, section)| !section.is_null()) {
            note_each(section, name, &mut applied);
        }
    }
    let settings = state.view(Caller::Owner, &state.stored());
    Ok((
        StatusCode::OK,
        Json(json!({ "settings": settings, "applied": applied })),
    )
        .into_response())
}

/// The body of `POST /api/settings/projects`: the folder to serve, as an absolute path.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AddProject {
    path: String,
}

/// `POST /api/settings/projects`. Only the owner may add a project. The folder is opened and its
/// first scan starts at once, with no restart. The folder is also kept in the settings, so the next
/// start serves it too. A failed open puts the settings back as they were.
///
/// Status codes: `202` with the new project's id, name and status `mapping`. `400` for a body with
/// no path. `401` for an unknown token. `403` for any token but the owner's. `409` when the folder
/// is already served, or the server was started without live adds. `422` with the reason when the
/// path is not an existing absolute folder, sits inside another project's state folder, or is inside
/// the public folder. `500` when the folder cannot be opened or the settings cannot be saved.
async fn post_project<C: ModelClient + 'static>(
    State(state): State<Arc<Settings<C>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    if state.caller(&headers, ONLY_OWNER_CHANGES)? != Caller::Owner {
        return Err(ApiError::Forbidden(ONLY_OWNER_CHANGES));
    }
    let request: AddProject = serde_json::from_slice(&body)
        .map_err(|_| ApiError::BadRequest("a JSON object with a path"))?;
    let _turn = state.changes.lock().await;
    let live = state
        .projects
        .live()
        .ok_or(ApiError::Conflict("live_add_unavailable"))?;
    let spec = add::check_new(&state.projects, &request.path, live.web.as_deref())?;
    let folder = spec.repo.display().to_string();
    let previous = state.stored();
    let mut next = previous.clone();
    next.projects.remove.retain(|kept| *kept != folder);
    if !next.projects.add.contains(&folder) {
        next.projects.add.push(folder);
    }
    file::save(&state.file, &next).map_err(|err| ApiError::Internal(err.into()))?;
    *state.stored.write().unwrap_or_else(PoisonError::into_inner) = next;
    match add::open(live, &state.projects, spec).await {
        Ok((id, name)) => Ok((
            StatusCode::ACCEPTED,
            Json(json!({ "id": id, "name": name, "status": "mapping" })),
        )
            .into_response()),
        Err(err) => {
            if let Err(restore) = file::save(&state.file, &previous) {
                tracing::error!(error = %restore, "restoring the settings after a failed add");
            }
            *state.stored.write().unwrap_or_else(PoisonError::into_inner) = previous;
            Err(ApiError::Internal(err))
        }
    }
}

/// `GET /api/settings/tokens/owner`. Only the owner may read the owner's token. It is returned
/// whole, the one place a read carries a token. `401` for an unknown token, `403` for any other.
async fn get_owner_token<C: ModelClient + 'static>(
    State(state): State<Arc<Settings<C>>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.owner_only(&headers, ONLY_OWNER_TOKENS)?;
    let token = state
        .token_held_by(&Principal::Member(state.founder.clone()))
        .ok_or(ApiError::NotFound("owner_token"))?;
    Ok(Json(json!({ "token": token })))
}

/// `POST /api/settings/tokens/owner/rotate`. Only the owner may rotate the owner's token. The new
/// token replaces the old one in every project, and the old one stops working at once. A
/// connection that holds the old token is closed. The response carries the new token only.
///
/// When a project's tokens file cannot be written, the rotation stops there: the projects before
/// it hold the new token, and the others still hold the old one. The response is `500` and names
/// the step that failed. It never claims success, and it never carries a token.
async fn rotate_owner_token<C: ModelClient + 'static>(
    State(state): State<Arc<Settings<C>>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    state.owner_only(&headers, ONLY_OWNER_TOKENS)?;
    let _turn = state.changes.lock().await;
    let token = Tokens::mint_token();
    let owner = Principal::Member(state.founder.clone());
    for entry in state.projects.entries() {
        let dir = entry.host.repo().join(STATE_DIR);
        if let Err(err) = entry.host.tokens().replace_token(&dir, &owner, &token) {
            let step = format!("the owner token in project {}", entry.id);
            return Ok(rotation_failed(&step, &err));
        }
    }
    // The user's folder holds the owner's token too, so the old token stops working there as well.
    if let Some(home) = state.projects.home()
        && let Err(err) = home.tokens.replace_token(&home.dir, &owner, &token)
    {
        return Ok(rotation_failed(
            "the owner token in the user's folder",
            &err,
        ));
    }
    Ok((StatusCode::OK, Json(json!({ "token": token }))).into_response())
}

/// `POST /api/settings/tokens/agents/rotate`. Only the owner may rotate the agents' tokens. Each
/// agent in each project gets a new token, and its old one stops working at once. A connection
/// that holds an old token is closed. The response counts the tokens replaced: one for each agent
/// in each project.
///
/// A failure stops the rotation and returns `500` with the step that failed, as the owner rotation
/// does.
async fn rotate_agent_tokens<C: ModelClient + 'static>(
    State(state): State<Arc<Settings<C>>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    state.owner_only(&headers, ONLY_OWNER_TOKENS)?;
    let _turn = state.changes.lock().await;
    let mut rotated: usize = 0;
    for entry in state.projects.entries() {
        let dir = entry.host.repo().join(STATE_DIR);
        for agent in entry.host.tokens().agents() {
            let token = Tokens::mint_token();
            let principal = Principal::Agent(agent.clone());
            if let Err(err) = entry.host.tokens().replace_token(&dir, &principal, &token) {
                let step = format!(
                    "the token of agent {} in project {}",
                    agent.as_str(),
                    entry.id
                );
                return Ok(rotation_failed(&step, &err));
            }
            rotated += 1;
        }
    }
    Ok((StatusCode::OK, Json(json!({ "rotated": rotated }))).into_response())
}

/// The response for a rotation that stopped at `step`. The step names what failed. The error is
/// logged, and neither carries a token.
fn rotation_failed(step: &str, err: &anyhow::Error) -> Response {
    tracing::error!(error = %err, step, "a token rotation stopped partway");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "rotation_failed", "what": step })),
    )
        .into_response()
}

/// `GET /api/settings/update/check`. Only the owner may check. It reports the running version and
/// the newest pre-release on GitHub, and downloads nothing. GitHub being unreachable is a state.
///
/// Status codes: `200` with the check. `401` for an unknown token. `403` for any token but the
/// owner's.
async fn get_update_check<C: ModelClient + 'static>(
    State(state): State<Arc<Settings<C>>>,
    headers: HeaderMap,
) -> Result<Json<update::Check>, ApiError> {
    state.owner_only(&headers, ONLY_OWNER_UPDATES)?;
    Ok(Json(update::check(&update::Github::production()).await))
}

/// `POST /api/settings/update`. Only the owner may install an update. The newest pre-release's
/// asset for this install is downloaded, and kept only when its SHA256 matches `SHA256SUMS`. On
/// Windows the verified setup then runs detached and silently, and the answer is `202` at once. On
/// Linux the verified archive is kept, and its path is reported.
///
/// Status codes: `202` with `{"status": "installing"}` on Windows. `200` with the path and version
/// of the verified download on Linux. `401` for an unknown token. `403` for any token but the
/// owner's. `409` with `no_flavor`, `offline`, `no_prerelease`, `no_asset` or `checksum_mismatch`
/// when the update cannot be verified, which installs nothing. `500` when the download cannot be
/// written or the setup cannot start.
async fn post_update<C: ModelClient + 'static>(
    State(state): State<Arc<Settings<C>>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    state.owner_only(&headers, ONLY_OWNER_UPDATES)?;
    let dir = update::install_dir().map_err(|err| ApiError::Internal(err.into()))?;
    let flavor = update::flavor_in(&dir).map_err(update_failure)?;
    let download = update::download_latest(
        &update::Github::production(),
        flavor,
        &update::download_dir(),
        cfg!(windows),
    )
    .await
    .map_err(update_failure)?;
    if cfg!(windows) {
        update::run_setup_detached(&download.path).map_err(|err| ApiError::Internal(err.into()))?;
        return Ok((StatusCode::ACCEPTED, Json(json!({"status": "installing"}))).into_response());
    }
    Ok((
        StatusCode::OK,
        Json(json!({
            "status": "downloaded",
            "path": download.path.display().to_string(),
            "version": download.version,
        })),
    )
        .into_response())
}

/// The response for an update that cannot be verified or downloaded. Each cause is a conflict
/// that names it.
fn update_failure(err: update::UpdateError) -> ApiError {
    match err {
        update::UpdateError::NoFlavor => ApiError::Conflict("no_flavor"),
        update::UpdateError::Offline => ApiError::Conflict("offline"),
        update::UpdateError::NoPrerelease => ApiError::Conflict("no_prerelease"),
        update::UpdateError::NoAsset => ApiError::Conflict("no_asset"),
        update::UpdateError::Checksum => ApiError::Conflict("checksum_mismatch"),
        update::UpdateError::Storage(err) => ApiError::Internal(err.into()),
    }
}

/// The settings routes. They are global, since the settings belong to the server and not to a
/// project.
pub(crate) fn routes<C: ModelClient + 'static>(state: Arc<Settings<C>>) -> Router {
    Router::new()
        .route(
            "/api/settings",
            get(get_settings::<C>).put(put_settings::<C>),
        )
        .route("/api/settings/projects", post(post_project::<C>))
        .route("/api/settings/update", post(post_update::<C>))
        .route("/api/settings/update/check", get(get_update_check::<C>))
        .route("/api/settings/tokens/owner", get(get_owner_token::<C>))
        .route(
            "/api/settings/tokens/owner/rotate",
            post(rotate_owner_token::<C>),
        )
        .route(
            "/api/settings/tokens/agents/rotate",
            post(rotate_agent_tokens::<C>),
        )
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_match_the_budgets_the_server_offers() {
        let budgets = Budgets::default();
        assert_eq!(budgets.repair_rounds, 2);
        assert_eq!(budgets.max_episodes, 4);
        assert_eq!(budgets.wall_time_ms, 600_000);
        assert_eq!(budgets.stuck_declare_ms, 300_000);
        assert_eq!(budgets.stuck_build_ms, 900_000);
        assert_eq!(budgets.drift_threshold, 50);
    }

    #[test]
    fn the_isometric_view_is_the_default_look() {
        let look = Look::default();
        assert!(!look.top_down_default);
        assert_eq!(look.theme, Theme::Paper);
        assert_eq!(look.label_density, LabelDensity::Normal);
    }

    #[test]
    fn the_settings_shown_leave_out_every_key_this_version_does_not_use() {
        let mut stored = Stored::default();
        stored.budgets.repair_rounds = 5;
        for unknown in [
            &mut stored.unknown,
            &mut stored.harness.unknown,
            &mut stored.projects.unknown,
            &mut stored.budgets.unknown,
            &mut stored.look.unknown,
            &mut stored.notifications.unknown,
        ] {
            unknown.insert("extra".to_owned(), json!("hidden-value"));
        }
        assert_eq!(stored.unknown_keys().len(), 6, "each level holds one");

        let shown = stored.without_unknown();
        assert!(shown.unknown_keys().is_empty());
        let text = serde_json::to_string(&shown).expect("the settings serialize");
        assert!(!text.contains("extra"), "shown: {text}");
        assert!(!text.contains("hidden-value"), "shown: {text}");
        let mut expected = Stored::default();
        expected.budgets.repair_rounds = 5;
        assert_eq!(shown, expected, "only the unknown keys are left out");
    }

    #[test]
    fn the_unknown_keys_warning_names_the_keys_and_no_value() {
        /// A log writer the test reads back.
        #[derive(Clone, Default)]
        struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

        impl std::io::Write for Captured {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("lock poisoned").extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .finish();
        let mut stored = Stored::default();
        stored
            .projects
            .unknown
            .insert("live".to_owned(), json!(["/some/folder"]));
        stored
            .look
            .unknown
            .insert("glow".to_owned(), json!("unshown-value"));
        tracing::subscriber::with_default(subscriber, || {
            warn_unknown_keys(&stored);
            warn_unknown_keys(&Stored::default());
        });

        let bytes = captured.0.lock().expect("lock poisoned").clone();
        let text = String::from_utf8(bytes).expect("the log is text");
        assert_eq!(
            text.lines().count(),
            1,
            "one warning, none for no keys: {text}"
        );
        assert!(text.contains("projects.live, look.glow"), "log: {text}");
        assert!(!text.contains("/some/folder"), "log: {text}");
        assert!(!text.contains("unshown-value"), "log: {text}");
    }

    #[test]
    fn a_harness_role_with_no_entry_runs_the_builtin_adapter() {
        let view = HarnessView::of(&Harness::default());
        assert_eq!(view.roles.len(), Role::ALL.len());
        assert!(
            view.roles
                .values()
                .all(|adapter| *adapter == Adapter::Builtin)
        );
    }

    #[test]
    fn each_setting_says_how_it_takes_effect() {
        assert_eq!(applied_label("harness.roles.foreman"), "stored");
        assert_eq!(applied_label("harness.custom_command"), "stored");
        assert_eq!(applied_label("budgets.repair_rounds"), "stored");
        assert_eq!(applied_label("projects.add"), "restart");
        assert_eq!(applied_label("projects.remove"), "restart");
        assert_eq!(applied_label("look.theme"), "client");
        assert_eq!(applied_label("notifications.sound"), "client");
    }

    #[test]
    fn a_change_names_each_setting_it_sets_by_its_path() {
        let mut applied = BTreeMap::new();
        note_each(
            &json!({"roles": {"foreman": "codex"}, "custom_command": ["run"]}),
            "harness",
            &mut applied,
        );
        assert_eq!(
            applied,
            BTreeMap::from([
                ("harness.custom_command".to_owned(), "stored"),
                ("harness.roles.foreman".to_owned(), "stored"),
            ])
        );
    }

    #[test]
    fn a_masked_token_shows_its_first_five_and_last_four_characters() {
        assert_eq!(mask("acan_0123456789abcdef"), "acan_…cdef");
        assert_eq!(mask("short"), "…", "a short token is hidden entirely");
    }
}
