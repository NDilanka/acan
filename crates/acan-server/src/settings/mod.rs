//! The server's settings: the harness, the projects, the budgets, the look and the notifications.
//! The model keys are not here. They stay in the keys file the setup page keeps, and this module
//! reaches them only through the model client, so no response and no settings file holds a key.
//!
//! `GET /api/settings` reads the settings. Any member may read them. `PUT /api/settings` changes
//! them, and only the owner may do that. A change is checked as a whole before anything is written,
//! so a change that fails leaves every setting as it was. See [`validate`] for the rules.

pub(crate) mod data;
pub(crate) mod file;
mod validate;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Instant;

use acan_core::{AgentId, MemberId};
use acan_llm::{
    ChatRequest, KeyOrigin, KeyOutcome, KeySource, KeyStatus, LlmError, Message, ModelClient, Role,
    RoleStatus,
};
use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::STATE_DIR;
use crate::auth::{Principal, Tokens};
use crate::host::Host;
use crate::http::{ApiError, bearer};
use crate::registry::Registry;
use crate::release::{self, VersionInfo};
use crate::setup::setup_error;

pub(crate) use file::load;
use validate::{KeyChange, ModelsChange, Patch};

/// The file the settings are kept in, beside the keys file.
pub(crate) const FILE: &str = "settings.json";

/// The version of the settings document `GET /api/settings` returns.
const DOCUMENT_VERSION: u32 = 1;

/// Why an agent may not read the settings.
const ONLY_MEMBERS_READ: &str = "only members may read settings";

/// Why anyone but the owner may not change the settings.
const ONLY_OWNER_CHANGES: &str = "only the owner may change settings";

/// Why anyone but the owner may not read or rotate a token.
const ONLY_OWNER_TOKENS: &str = "only the owner may manage tokens";

/// Why anyone but the owner may not test a model.
const ONLY_OWNER_TESTS: &str = "only the owner may test a model";

/// The prompt a model test sends. It asks for the smallest reply, so a test costs little.
const TEST_PROMPT: &str = "Reply with the word ok.";

/// The most tokens a model test's reply may take.
const TEST_MAX_TOKENS: u32 = 16;

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

/// What the settings file holds. Model keys are never here, since they live in the keys file.
/// A missing section, or field, reads as its default.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Stored {
    pub(crate) harness: Harness,
    pub(crate) projects: Projects,
    pub(crate) budgets: Budgets,
    pub(crate) look: Look,
    pub(crate) notifications: Notifications,
}

/// The adapter each role runs through, and the command the `custom` adapter runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Harness {
    /// The adapter of each role that has one. A role with no entry runs the `builtin` adapter.
    pub(crate) roles: BTreeMap<Role, Adapter>,
    /// The command the `custom` adapter runs, one argument per entry. Empty when none is set.
    pub(crate) custom_command: Vec<String>,
}

/// The projects a change records. Adds and removals take effect at the next start, and stay in
/// effect until a later change undoes them. The worker counts are what each project runs with.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Projects {
    /// The local worker count of each project, by id. A project with no entry runs the count the
    /// server started with.
    pub(crate) workers: BTreeMap<String, u8>,
    /// Folders served as projects beyond the ones the command line or projects file names, as
    /// absolute paths.
    pub(crate) add: Vec<String>,
    /// Folders of projects that are not served, as absolute paths. A folder stays here until a
    /// change adds it again.
    pub(crate) remove: Vec<String>,
    /// Whether each project uses the LLM refinement of scanned state charts, by id. A project with
    /// no entry does not use it. This takes effect at once, since the flag is read on each use.
    pub(crate) statemap_llm: BTreeMap<String, bool>,
}

/// The limits the budgets set. The repair rounds and the call limits are the ones the server
/// offers; see [`Budgets::default`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Budgets {
    /// The repair rounds an order gets. Matches the budget acan-core offers before the first
    /// confirmation of an order.
    pub(crate) repair_rounds: u32,
    /// The new episodes an order may open.
    pub(crate) max_episodes: u32,
    /// The wall time a repair may take, in milliseconds.
    pub(crate) wall_time_ms: u64,
    /// The model cost a repair may reach, in micro-dollars.
    pub(crate) model_cost_micros: u64,
    /// The most calls each role may make.
    pub(crate) call_limits: CallLimits,
    /// How long a task may declare with no progress before it is stuck, in milliseconds.
    pub(crate) stuck_declare_ms: u64,
    /// How long a build may run with no progress before it is stuck, in milliseconds.
    pub(crate) stuck_build_ms: u64,
    /// How far a plant may drift before it is flagged, in percent.
    pub(crate) drift_threshold: u32,
}

impl Default for Budgets {
    fn default() -> Self {
        Self {
            repair_rounds: 2,
            max_episodes: 4,
            wall_time_ms: 600_000,
            model_cost_micros: 2_000_000,
            call_limits: CallLimits::default(),
            stuck_declare_ms: 300_000,
            stuck_build_ms: 900_000,
            drift_threshold: 50,
        }
    }
}

/// The most calls one role may make, by role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct CallLimits {
    pub(crate) foreman: u32,
    pub(crate) worker: u32,
    pub(crate) fast: u32,
    pub(crate) vision: u32,
    pub(crate) director: u32,
    pub(crate) wonder: u32,
    pub(crate) statemap: u32,
}

/// The call limit every role starts with.
const DEFAULT_CALL_LIMIT: u32 = 30;

impl Default for CallLimits {
    fn default() -> Self {
        Self {
            foreman: DEFAULT_CALL_LIMIT,
            worker: DEFAULT_CALL_LIMIT,
            fast: DEFAULT_CALL_LIMIT,
            vision: DEFAULT_CALL_LIMIT,
            director: DEFAULT_CALL_LIMIT,
            wonder: DEFAULT_CALL_LIMIT,
            statemap: DEFAULT_CALL_LIMIT,
        }
    }
}

impl CallLimits {
    /// Sets the call limit of `role`.
    pub(crate) fn set(&mut self, role: Role, limit: u32) {
        match role {
            Role::Foreman => self.foreman = limit,
            Role::Worker => self.worker = limit,
            Role::Fast => self.fast = limit,
            Role::Vision => self.vision = limit,
            Role::Director => self.director = limit,
            Role::Wonder => self.wonder = limit,
            Role::Statemap => self.statemap = limit,
        }
    }
}

/// How the page looks. The server stores these and the page applies them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Look {
    pub(crate) theme: Theme,
    /// Whether the page opens top-down. The isometric view is the default.
    pub(crate) top_down_default: bool,
    pub(crate) motion: bool,
    pub(crate) idle_bob: bool,
    pub(crate) reduced_motion: bool,
    pub(crate) label_density: LabelDensity,
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
        }
    }
}

/// What the page notifies the tester about. The server stores these and the page applies them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Notifications {
    pub(crate) needs_you: bool,
    pub(crate) sound: bool,
    pub(crate) browser: bool,
}

impl Default for Notifications {
    fn default() -> Self {
        Self {
            needs_you: true,
            sound: false,
            browser: false,
        }
    }
}

/// A part of the settings that a change names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Section {
    Models,
    Harness,
    Projects,
    Budgets,
    Look,
    Notifications,
}

/// How the setting at `path` takes effect, as the `applied` map of a response and `GET` name it.
/// The path is dotted, as a request names the setting. `live` means the server uses it at once.
/// `stored` means it is saved, and no server path reads it yet. `restart` means it takes effect at
/// the next start. `client` means the page applies it. A project's statemap flag is read on each
/// use, so it is `live`. So is `budgets.call_limits.statemap`, once the statemap pass reads it on
/// each scan.
fn applied_label(path: &str) -> &'static str {
    let mut parts = path.split('.');
    match (parts.next(), parts.next(), parts.next()) {
        (Some("models"), _, _) => "live",
        (Some("harness"), _, _) => "stored",
        (Some("projects"), Some("statemap_llm"), _) => "live",
        (Some("projects"), _, _) => "restart",
        (Some("budgets"), Some("call_limits"), Some("statemap")) => "live",
        (Some("budgets"), _, _) => "stored",
        (Some("look" | "notifications"), _, _) => "client",
        _ => "stored",
    }
}

/// Records the setting at `path`, and how it takes effect.
fn note(applied: &mut BTreeMap<String, &'static str>, path: String) {
    let label = applied_label(&path);
    applied.insert(path, label);
}

/// Records each setting under `value`, at its dotted `path`. An object's fields are walked, so each
/// budget limit is its own setting. A provider's key is one setting however it is written, so
/// `models.keys` names each provider. Any other value, arrays included, is one setting.
fn note_each(value: &Value, path: &str, applied: &mut BTreeMap<String, &'static str>) {
    match value {
        Value::Object(fields) if path == "models.keys" => {
            for name in fields.keys() {
                note(applied, format!("{path}.{name}"));
            }
        }
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
    /// The projects. The default project's client holds the model keys, and every project's tokens
    /// may authenticate a request.
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

    /// The principal the request's bearer token acts for. Each project keeps its own tokens, so
    /// every project's table is asked.
    fn principal(&self, headers: &HeaderMap) -> Result<Principal, ApiError> {
        let token = bearer(headers).ok_or(ApiError::Unauthorized)?;
        self.projects
            .entries()
            .iter()
            .find_map(|entry| entry.host.tokens().principal(token))
            .ok_or(ApiError::Unauthorized)
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

    /// The project whose client holds the model keys: the default project.
    fn default_host(&self) -> &Host<C> {
        let entries = self.projects.entries();
        let default = entries
            .iter()
            .find(|entry| entry.id == self.projects.default_id())
            .unwrap_or(&entries[0]);
        &default.host
    }

    /// The document `GET /api/settings` returns to `caller`. It holds no key.
    fn view(&self, caller: Caller, stored: &Stored) -> View {
        View {
            version: DOCUMENT_VERSION,
            you: match caller {
                Caller::Owner => "owner",
                Caller::Member => "member",
            },
            editable: caller == Caller::Owner,
            models: ModelsView::of(self.default_host().client().key_status()),
            harness: HarnessView::of(&stored.harness),
            projects: self.projects_view(stored),
            budgets: stored.budgets.clone(),
            look: stored.look.clone(),
            notifications: stored.notifications.clone(),
            applied: self.applied_fields(),
            project_changes: (caller == Caller::Owner).then(|| ProjectChangesView {
                add: stored.projects.add.clone(),
                remove: stored.projects.remove.clone(),
            }),
            tokens: (caller == Caller::Owner).then(|| self.tokens_view()),
            about: release::version_info(),
        }
    }

    /// How each setting a change can name takes effect, by its dotted path. The projects listed
    /// are the running ones, as `GET` lists them.
    fn applied_fields(&self) -> BTreeMap<String, &'static str> {
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
            note(&mut applied, format!("models.roles.{role}"));
            note(&mut applied, format!("harness.roles.{role}"));
        }
        note(&mut applied, "harness.custom_command".to_owned());
        for provider in self.default_host().client().key_status().providers {
            note(&mut applied, format!("models.keys.{}", provider.name));
        }
        note(&mut applied, "projects.add".to_owned());
        note(&mut applied, "projects.remove".to_owned());
        for entry in self.projects.entries() {
            note(&mut applied, format!("projects.workers.{}", entry.id));
            note(&mut applied, format!("projects.statemap_llm.{}", entry.id));
        }
        applied
    }

    /// Each running project, with its worker count: the stored count, or the one it started with.
    fn projects_view(&self, stored: &Stored) -> Vec<ProjectView> {
        self.projects
            .entries()
            .iter()
            .map(|entry| ProjectView {
                id: entry.id.clone(),
                name: entry.name.clone(),
                workers: worker_count(stored, &entry.id, entry.board.view().workers.len()),
                statemap_llm: stored
                    .projects
                    .statemap_llm
                    .get(&entry.id)
                    .copied()
                    .unwrap_or(false),
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
    models: ModelsView,
    harness: HarnessView,
    projects: Vec<ProjectView>,
    budgets: Budgets,
    look: Look,
    notifications: Notifications,
    /// How each setting takes effect, by its dotted path: `live`, `stored`, `restart` or `client`.
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

/// The model keys and role picks, as the client reports them. Never a key.
#[derive(Debug, Serialize)]
struct ModelsView {
    configured: bool,
    can_save: bool,
    source: Option<KeySource>,
    keys: Vec<KeyView>,
    roles: Vec<RoleStatus>,
}

impl ModelsView {
    fn of(status: KeyStatus) -> Self {
        Self {
            configured: status.configured,
            can_save: status.can_save,
            source: status.source,
            keys: status
                .providers
                .into_iter()
                .map(|provider| KeyView {
                    set: provider.source.is_some(),
                    source: provider.source,
                    provider: provider.name,
                })
                .collect(),
            roles: status.roles,
        }
    }
}

/// Whether one provider has a key, and where the key came from.
#[derive(Debug, Serialize)]
struct KeyView {
    provider: String,
    set: bool,
    source: Option<KeyOrigin>,
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
    workers: usize,
    /// Whether the project uses the LLM refinement of scanned state charts.
    statemap_llm: bool,
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
/// failing field changes nothing. The model keys are checked and saved through the client, and each
/// key's result is reported. The other sections are written to the settings file.
///
/// Status codes: `200` with the settings after the change, how each changed setting takes effect
/// by its dotted path, and the result of each key. `400` for a body that is not a settings object, or names a section or field
/// the server does not take. `401` for an unknown token. `403` for any token but the owner's. `409`
/// when the model keys cannot be changed here, or the current state refuses the change. `422` with
/// each failing field. `500` when the settings file cannot be written.
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
    let running: Vec<(&str, PathBuf)> = state
        .projects
        .entries()
        .iter()
        .map(|entry| (entry.id.as_str(), entry.host.repo().to_path_buf()))
        .collect();
    let changes = validate::check(
        &patch,
        &state.stored(),
        &running,
        state.projects.default_id(),
    )
    .map_err(ApiError::Invalid)?;

    let host = state.default_host();
    let mut keys = BTreeMap::new();
    if let Some(models) = &changes.models {
        refuse_model_conflicts(&host.client().key_status(), models)?;
        keys = apply_models(host, models).await?;
    }
    if changes
        .sections
        .iter()
        .any(|section| *section != Section::Models)
    {
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
        Json(json!({ "settings": settings, "applied": applied, "keys": keys })),
    )
        .into_response())
}

/// Refuses a model change the current state does not allow, before any key is checked or saved.
/// A key the change sets may still be refused by its provider, so a role pick for that provider
/// is allowed here and checked when it is applied.
fn refuse_model_conflicts(status: &KeyStatus, models: &ModelsChange) -> Result<(), ApiError> {
    if models.is_empty() {
        return Ok(());
    }
    if !status.can_save {
        let why = if status.source == Some(KeySource::RoutingFile) {
            "routing_file"
        } else {
            "unavailable"
        };
        return Err(ApiError::Conflict(why));
    }
    for (provider, change) in &models.keys {
        let from_environment = status
            .providers
            .iter()
            .any(|known| known.name == *provider && known.source == Some(KeyOrigin::Environment));
        if matches!(change, KeyChange::Clear) && from_environment {
            return Err(ApiError::Conflict("this key comes from the environment"));
        }
    }
    for (_, pick) in &models.roles {
        let Some(provider) = pick else {
            continue;
        };
        let set_now = models
            .keys
            .iter()
            .any(|(name, change)| name == provider && matches!(change, KeyChange::Set(_)));
        let cleared = models
            .keys
            .iter()
            .any(|(name, change)| name == provider && matches!(change, KeyChange::Clear));
        let held = status
            .providers
            .iter()
            .any(|known| known.name == *provider && (known.saved || known.environment));
        if !(set_now || (held && !cleared)) {
            return Err(ApiError::Conflict("no_key"));
        }
    }
    Ok(())
}

/// Saves, checks and clears the model keys, then picks the role providers, through the client.
/// Returns each key's result: `stored`, `rejected` or `unreachable` for a key that was checked, and
/// `cleared` for a key that was removed.
async fn apply_models<C: ModelClient>(
    host: &Host<C>,
    models: &ModelsChange,
) -> Result<BTreeMap<String, &'static str>, ApiError> {
    let client = host.client();
    let mut results = BTreeMap::new();
    for (provider, change) in &models.keys {
        let result = match change {
            KeyChange::Set(key) => {
                match client.save_key(provider, key).await.map_err(setup_error)? {
                    KeyOutcome::Stored { .. } => "stored",
                    KeyOutcome::Rejected => "rejected",
                    KeyOutcome::Unreachable => "unreachable",
                }
            }
            KeyChange::Clear => {
                client.remove_key(provider).map_err(setup_error)?;
                "cleared"
            }
        };
        results.insert(provider.clone(), result);
    }
    for (role, pick) in &models.roles {
        client
            .set_role(*role, pick.as_deref())
            .map_err(setup_error)?;
    }
    Ok(results)
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

/// The result of a model test. Its `error` is one of a fixed set of names, so no text a provider
/// sent, no key and no URL is ever returned.
#[derive(Debug, Serialize)]
struct RoleTest {
    role: &'static str,
    ok: bool,
    provider: Option<String>,
    model: Option<String>,
    ms: u64,
    error: Option<&'static str>,
}

impl RoleTest {
    /// The result for a role with no route. No provider can answer it, so no request is sent.
    fn no_key(role: Role) -> Self {
        Self {
            role: role.as_str(),
            ok: false,
            provider: None,
            model: None,
            ms: 0,
            error: Some("no_key"),
        }
    }
}

/// The fixed name a failed model test reports. The provider's text, a key and a URL are dropped.
fn test_error(err: &LlmError) -> &'static str {
    match err {
        LlmError::MissingKey { .. } | LlmError::RoleOff { .. } => "no_key",
        LlmError::Timeout { .. } => "timeout",
        LlmError::Transport { .. } | LlmError::ServerError { .. } => "unreachable",
        LlmError::PaymentRequired { .. }
        | LlmError::Status {
            status: 401 | 403, ..
        } => "rejected",
        _ => "failed",
    }
}

/// `POST /api/settings/models/roles/{role}/test`. Only the owner may test a role's model. The
/// test sends one short prompt to the role's provider, and reports whether it answered, which
/// provider and model it asked, and how long it took. A role with no route has no provider to ask,
/// so no request is sent and the error is `no_key`.
///
/// Status codes: `200` with the result. `401` for an unknown token, `403` for any token but the
/// owner's, and `404` with `what: "role"` for a role the server does not have.
async fn test_role<C: ModelClient + 'static>(
    State(state): State<Arc<Settings<C>>>,
    UrlPath(name): UrlPath<String>,
    headers: HeaderMap,
) -> Result<Json<RoleTest>, ApiError> {
    state.owner_only(&headers, ONLY_OWNER_TESTS)?;
    let role = name
        .parse::<Role>()
        .map_err(|_| ApiError::NotFound("role"))?;
    let client = state.default_host().client();
    let Some(destination) = client.destination(role) else {
        return Ok(Json(RoleTest::no_key(role)));
    };
    let request = ChatRequest {
        messages: vec![Message::user(TEST_PROMPT)],
        temperature: Some(0.0),
        max_tokens: Some(TEST_MAX_TOKENS),
        ..ChatRequest::default()
    };
    let started = Instant::now();
    let outcome = client.chat(role, request).await;
    let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let error = outcome.err().map(|err| test_error(&err));
    Ok(Json(RoleTest {
        role: role.as_str(),
        ok: error.is_none(),
        provider: Some(destination.provider),
        model: Some(destination.model),
        ms,
        error,
    }))
}

/// The settings routes. They are global, since the settings belong to the server and not to a
/// project.
pub(crate) fn routes<C: ModelClient + 'static>(state: Arc<Settings<C>>) -> Router {
    Router::new()
        .route(
            "/api/settings",
            get(get_settings::<C>).put(put_settings::<C>),
        )
        .route("/api/settings/tokens/owner", get(get_owner_token::<C>))
        .route(
            "/api/settings/tokens/owner/rotate",
            post(rotate_owner_token::<C>),
        )
        .route(
            "/api/settings/tokens/agents/rotate",
            post(rotate_agent_tokens::<C>),
        )
        .route(
            "/api/settings/models/roles/{role}/test",
            post(test_role::<C>),
        )
        .with_state(state)
}

/// Whether the LLM refinement of scanned state charts is on for project `project_id`, as the
/// settings file at `settings_path` says now. The file is read on each call, so a change takes
/// effect at once. A missing file, or a project with no entry, is off. A file that cannot be read
/// is off too, and the warning names no part of it.
pub(crate) fn statemap_llm_enabled(settings_path: &Path, project_id: &str) -> bool {
    match file::load(settings_path) {
        Ok(stored) => stored
            .projects
            .statemap_llm
            .get(project_id)
            .copied()
            .unwrap_or(false),
        Err(err) => {
            tracing::warn!(error = %err, "the statemap LLM setting cannot be read; it stays off");
            false
        }
    }
}

/// The most calls the statemap role may make, as the settings file at `settings_path` says now.
/// The file is read on each call, like [`statemap_llm_enabled`]. A missing file, or one that
/// cannot be read, gives the default limit.
pub(crate) fn statemap_call_limit(settings_path: &Path) -> u32 {
    match file::load(settings_path) {
        Ok(stored) => stored.budgets.call_limits.statemap,
        Err(err) => {
            tracing::warn!(error = %err, "the statemap call limit cannot be read; the default applies");
            DEFAULT_CALL_LIMIT
        }
    }
}

/// The local worker count of project `id`: the count the settings give it, or `started` when they
/// give none. The count is read when the server starts, so a change takes effect at the next start.
pub(crate) fn worker_count(stored: &Stored, id: &str, started: usize) -> usize {
    stored
        .projects
        .workers
        .get(id)
        .map_or(started, |count| usize::from(*count))
}

#[cfg(test)]
mod tests {
    use acan_llm::ProviderStatus;

    use super::*;

    #[test]
    fn a_project_with_no_worker_count_runs_the_count_it_started_with() {
        let mut stored = Stored::default();
        assert_eq!(worker_count(&stored, "alpha", 2), 2);
        stored.projects.workers.insert("alpha".to_owned(), 0);
        assert_eq!(worker_count(&stored, "alpha", 2), 0, "zero runs none");
        assert_eq!(
            worker_count(&stored, "beta", 2),
            2,
            "the others keep theirs"
        );
    }

    #[test]
    fn the_statemap_call_limit_is_read_from_the_file_on_each_call() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(FILE);
        assert_eq!(
            statemap_call_limit(&path),
            30,
            "a missing file gives the default"
        );

        let mut stored = Stored::default();
        stored.budgets.call_limits.statemap = 12;
        file::save(&path, &stored).expect("the settings are saved");
        assert_eq!(statemap_call_limit(&path), 12);

        std::fs::write(&path, "{ not json").expect("the file is written");
        assert_eq!(
            statemap_call_limit(&path),
            30,
            "a file that does not parse gives the default"
        );
    }

    /// A status where each `(provider, saved, environment)` has a key, and the setup saved the keys.
    fn status(providers: &[(&str, bool, bool)]) -> KeyStatus {
        KeyStatus {
            configured: true,
            source: Some(KeySource::Setup),
            can_save: true,
            providers: providers
                .iter()
                .map(|(name, saved, environment)| ProviderStatus::new(name, *saved, *environment))
                .collect(),
            roles: Vec::new(),
        }
    }

    fn pick(provider: &str) -> ModelsChange {
        ModelsChange {
            keys: Vec::new(),
            roles: vec![(Role::Foreman, Some(provider.to_owned()))],
        }
    }

    #[test]
    fn a_role_pick_needs_a_key_for_its_provider() {
        let saved = status(&[("meta", true, false), ("baseten", false, false)]);
        assert!(refuse_model_conflicts(&saved, &pick("meta")).is_ok());
        assert!(matches!(
            refuse_model_conflicts(&saved, &pick("baseten")),
            Err(ApiError::Conflict("no_key"))
        ));
    }

    #[test]
    fn a_role_pick_for_a_key_set_in_the_same_change_is_allowed() {
        let saved = status(&[("baseten", false, false)]);
        let change = ModelsChange {
            keys: vec![("baseten".to_owned(), KeyChange::Set("typed-key".to_owned()))],
            roles: vec![(Role::Foreman, Some("baseten".to_owned()))],
        };
        assert!(refuse_model_conflicts(&saved, &change).is_ok());
    }

    #[test]
    fn a_role_pick_for_a_key_cleared_in_the_same_change_is_refused() {
        let saved = status(&[("meta", true, false)]);
        let change = ModelsChange {
            keys: vec![("meta".to_owned(), KeyChange::Clear)],
            roles: vec![(Role::Foreman, Some("meta".to_owned()))],
        };
        assert!(matches!(
            refuse_model_conflicts(&saved, &change),
            Err(ApiError::Conflict("no_key"))
        ));
    }

    #[test]
    fn an_environment_key_cannot_be_cleared_here() {
        let environment = status(&[("meta", false, true)]);
        let change = ModelsChange {
            keys: vec![("meta".to_owned(), KeyChange::Clear)],
            roles: Vec::new(),
        };
        assert!(matches!(
            refuse_model_conflicts(&environment, &change),
            Err(ApiError::Conflict("this key comes from the environment"))
        ));
    }

    #[test]
    fn a_routing_file_keeps_its_own_keys() {
        let routing = KeyStatus {
            configured: true,
            source: Some(KeySource::RoutingFile),
            can_save: false,
            providers: Vec::new(),
            roles: Vec::new(),
        };
        assert!(matches!(
            refuse_model_conflicts(&routing, &pick("meta")),
            Err(ApiError::Conflict("routing_file"))
        ));
    }

    #[test]
    fn a_change_with_no_model_part_is_never_refused_for_models() {
        let routing = KeyStatus::default();
        assert!(refuse_model_conflicts(&routing, &ModelsChange::default()).is_ok());
    }

    #[test]
    fn the_defaults_match_the_budgets_the_server_offers() {
        let budgets = Budgets::default();
        assert_eq!(budgets.repair_rounds, 2);
        assert_eq!(budgets.max_episodes, 4);
        assert_eq!(budgets.wall_time_ms, 600_000);
        assert_eq!(budgets.model_cost_micros, 2_000_000);
        assert_eq!(budgets.call_limits, CallLimits::default());
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
        assert_eq!(applied_label("models.roles.foreman"), "live");
        assert_eq!(applied_label("models.keys.fireworks"), "live");
        assert_eq!(applied_label("harness.roles.foreman"), "stored");
        assert_eq!(applied_label("harness.custom_command"), "stored");
        assert_eq!(applied_label("budgets.repair_rounds"), "stored");
        assert_eq!(applied_label("budgets.call_limits.worker"), "stored");
        assert_eq!(applied_label("budgets.call_limits.statemap"), "live");
        assert_eq!(applied_label("projects.add"), "restart");
        assert_eq!(applied_label("projects.remove"), "restart");
        assert_eq!(applied_label("projects.workers.acan-wip"), "restart");
        assert_eq!(applied_label("projects.statemap_llm.acan-wip"), "live");
        assert_eq!(applied_label("look.theme"), "client");
        assert_eq!(applied_label("notifications.sound"), "client");
    }

    #[test]
    fn a_change_names_each_setting_it_sets_by_its_path() {
        let mut applied = BTreeMap::new();
        note_each(
            &json!({"repair_rounds": 3, "call_limits": {"statemap": 9, "worker": 40}}),
            "budgets",
            &mut applied,
        );
        assert_eq!(
            applied,
            BTreeMap::from([
                ("budgets.call_limits.statemap".to_owned(), "live"),
                ("budgets.call_limits.worker".to_owned(), "stored"),
                ("budgets.repair_rounds".to_owned(), "stored"),
            ])
        );

        let mut applied = BTreeMap::new();
        note_each(
            &json!({"workers": {"acan-wip": 1}, "statemap_llm": {"acan-wip": true}}),
            "projects",
            &mut applied,
        );
        assert_eq!(
            applied,
            BTreeMap::from([
                ("projects.statemap_llm.acan-wip".to_owned(), "live"),
                ("projects.workers.acan-wip".to_owned(), "restart"),
            ])
        );

        let mut applied = BTreeMap::new();
        note_each(
            &json!({"keys": {"fireworks": {"clear": true}}, "roles": {"foreman": null}}),
            "models",
            &mut applied,
        );
        assert_eq!(
            applied,
            BTreeMap::from([
                ("models.keys.fireworks".to_owned(), "live"),
                ("models.roles.foreman".to_owned(), "live"),
            ])
        );
    }

    #[test]
    fn the_statemap_role_has_a_call_limit_that_defaults_to_thirty() {
        let mut limits = CallLimits::default();
        assert_eq!(limits.statemap, 30);
        limits.set(Role::Statemap, 12);
        assert_eq!(limits.statemap, 12);
        assert_eq!(limits.wonder, 30, "setting one role leaves the others");
    }

    #[test]
    fn a_masked_token_shows_its_first_five_and_last_four_characters() {
        assert_eq!(mask("acan_0123456789abcdef"), "acan_…cdef");
        assert_eq!(mask("short"), "…", "a short token is hidden entirely");
    }

    #[test]
    fn a_failed_model_test_reports_one_of_the_fixed_names() {
        let provider = || "a-provider".to_owned();
        assert_eq!(
            test_error(&LlmError::Timeout {
                provider: provider()
            }),
            "timeout"
        );
        assert_eq!(
            test_error(&LlmError::MissingKey {
                provider: provider(),
                env_var: "A_KEY".to_owned(),
            }),
            "no_key"
        );
        assert_eq!(
            test_error(&LlmError::RoleOff {
                role: Role::Statemap,
                reason: "no key",
            }),
            "no_key"
        );
        assert_eq!(
            test_error(&LlmError::Status {
                provider: provider(),
                status: 401,
                body: String::new(),
            }),
            "rejected"
        );
        assert_eq!(
            test_error(&LlmError::Status {
                provider: provider(),
                status: 500,
                body: String::new(),
            }),
            "failed"
        );
        assert_eq!(
            test_error(&LlmError::ServerError {
                provider: provider(),
                status: 503,
                attempts: 3,
            }),
            "unreachable"
        );
        assert_eq!(
            test_error(&LlmError::NoScriptedReply { role: Role::Fast }),
            "failed"
        );
    }

    #[test]
    fn the_statemap_llm_flag_is_read_from_the_file_on_each_call() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(FILE);
        assert!(
            !statemap_llm_enabled(&path, "acan-wip"),
            "a missing file is off"
        );

        let mut stored = Stored::default();
        stored
            .projects
            .statemap_llm
            .insert("acan-wip".to_owned(), true);
        file::save(&path, &stored).expect("the settings are saved");
        assert!(statemap_llm_enabled(&path, "acan-wip"));
        assert!(
            !statemap_llm_enabled(&path, "another-project"),
            "a project with no entry is off"
        );

        stored
            .projects
            .statemap_llm
            .insert("acan-wip".to_owned(), false);
        file::save(&path, &stored).expect("the settings are saved");
        assert!(
            !statemap_llm_enabled(&path, "acan-wip"),
            "a flag set to false is off"
        );

        std::fs::write(&path, "{ not json").expect("the file is written");
        assert!(
            !statemap_llm_enabled(&path, "acan-wip"),
            "a file that does not parse is off"
        );
    }
}
