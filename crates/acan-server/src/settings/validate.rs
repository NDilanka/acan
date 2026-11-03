//! Checks a settings change before anything is written. Every field is checked, and then the rules
//! that span sections, so a change that fails reports every failure at once.
//!
//! The only read here is whether a folder a change adds exists. Nothing here saves a key, writes a
//! file or changes a project. A failure names the field and the rule, and never the value sent, so
//! a key cannot come back in an error.

use std::collections::BTreeMap;
use std::path::Path;

use acan_intent::RepairRounds;
use acan_llm::{MUSE_SPARK_PROVIDER, Role, is_provider};
use serde::Deserialize;

use super::{
    Adapter, Budgets, Harness, LabelDensity, Look, Notifications, Projects, Section, Stored, Theme,
};
use crate::http::FieldError;
use crate::projects::valid_id;

/// The longest key a change may set, in bytes. acan-llm refuses a longer key when it saves one.
const MAX_KEY_BYTES: usize = 512;

/// The most arguments a custom command may have.
const MAX_COMMAND_ARGS: usize = 64;

/// The longest argument a custom command may have, in bytes.
const MAX_ARG_BYTES: usize = 512;

/// The most local workers one project may run.
const MAX_WORKERS: u8 = 8;

/// A settings change as the request sends it. Each section is optional, and each field in a
/// section is optional, so a change names only what it sets. Unknown fields are refused when the
/// request is parsed, so `tokens`, `about` and `version` are never accepted.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Patch {
    pub(crate) models: Option<ModelsPatch>,
    pub(crate) harness: Option<HarnessPatch>,
    pub(crate) projects: Option<ProjectsPatch>,
    pub(crate) budgets: Option<BudgetsPatch>,
    pub(crate) look: Option<LookPatch>,
    pub(crate) notifications: Option<NotificationsPatch>,
}

/// The model part of a change: keys to save or clear, and role picks.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelsPatch {
    #[serde(default)]
    pub(crate) keys: BTreeMap<String, KeyPatch>,
    /// A role mapped to the provider it uses, or to `null` to clear the pick.
    #[serde(default)]
    pub(crate) roles: BTreeMap<String, Option<String>>,
}

/// One provider's key: either a key to save, or `clear` set to `true`. Not `Debug`, so a key cannot
/// be printed by accident.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeyPatch {
    key: Option<String>,
    clear: Option<bool>,
}

/// The harness part of a change.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HarnessPatch {
    roles: Option<BTreeMap<String, String>>,
    custom_command: Option<Vec<String>>,
}

/// The projects part of a change.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectsPatch {
    add: Option<Vec<String>>,
    remove: Option<Vec<String>>,
    workers: Option<BTreeMap<String, i64>>,
    /// Whether each named project uses the statemap LLM. The ids must be projects the server runs.
    statemap_llm: Option<BTreeMap<String, bool>>,
}

impl ProjectsPatch {
    /// Whether the change needs a restart: adds, removals and worker counts take effect at the
    /// next start. A change that names only `statemap_llm` takes effect at once.
    pub(crate) fn needs_restart(&self) -> bool {
        self.add.is_some() || self.remove.is_some() || self.workers.is_some()
    }
}

/// The budgets part of a change. Numbers are read as wide integers, so an out-of-range number is
/// refused as a field, not as a malformed body.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BudgetsPatch {
    repair_rounds: Option<i64>,
    max_episodes: Option<i64>,
    wall_time_ms: Option<i64>,
    model_cost_micros: Option<i64>,
    call_limits: Option<CallLimitsPatch>,
    stuck_declare_ms: Option<i64>,
    stuck_build_ms: Option<i64>,
    drift_threshold: Option<i64>,
}

/// The call limit of each role.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CallLimitsPatch {
    foreman: Option<i64>,
    worker: Option<i64>,
    fast: Option<i64>,
    vision: Option<i64>,
    director: Option<i64>,
    wonder: Option<i64>,
    statemap: Option<i64>,
}

impl CallLimitsPatch {
    /// The limit the change sets for `role`, if it sets one. The match names every role, so a role
    /// the server adds cannot be left out of a change.
    fn get(&self, role: Role) -> Option<i64> {
        match role {
            Role::Foreman => self.foreman,
            Role::Worker => self.worker,
            Role::Fast => self.fast,
            Role::Vision => self.vision,
            Role::Director => self.director,
            Role::Wonder => self.wonder,
            Role::Statemap => self.statemap,
        }
    }
}

/// The look part of a change.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LookPatch {
    theme: Option<String>,
    top_down_default: Option<bool>,
    motion: Option<bool>,
    idle_bob: Option<bool>,
    reduced_motion: Option<bool>,
    label_density: Option<String>,
}

/// The notifications part of a change.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NotificationsPatch {
    needs_you: Option<bool>,
    sound: Option<bool>,
    browser: Option<bool>,
}

/// A checked change: the settings after it, the model part, and the sections it names.
pub(crate) struct Changes {
    /// The settings as they stand once the change is written.
    pub(crate) next: Stored,
    /// The model part, when the change names one. It is applied through the client, not the file.
    pub(crate) models: Option<ModelsChange>,
    /// The sections the change names, in their fixed order.
    pub(crate) sections: Vec<Section>,
}

/// The model part of a change. It holds keys, so it is not `Debug`.
#[derive(Default)]
pub(crate) struct ModelsChange {
    /// Each provider's key change, in provider order.
    pub(crate) keys: Vec<(String, KeyChange)>,
    /// Each role's pick. `None` clears the pick.
    pub(crate) roles: Vec<(Role, Option<String>)>,
}

impl ModelsChange {
    /// Whether the change asks for nothing.
    pub(crate) fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.roles.is_empty()
    }
}

/// What one provider's key change does. A `Set` holds the key, so it is not `Debug`.
pub(crate) enum KeyChange {
    Set(String),
    Clear,
}

/// Checks `patch` against the settings `current`. `projects` lists the ids of the projects the
/// server runs, which a change may name. A change that fails returns every failure. A change that
/// passes returns the settings after it.
///
/// # Errors
///
/// Returns each failing field, with its path and the rule it broke. The settings are not changed.
pub(crate) fn check(
    patch: &Patch,
    current: &Stored,
    projects_running: &[&str],
) -> Result<Changes, Vec<FieldError>> {
    let mut errors = Vec::new();
    let mut next = current.clone();
    let mut sections = Vec::new();
    let mut models = None;
    if let Some(section) = &patch.models {
        sections.push(Section::Models);
        models = Some(models_change(section, &mut errors));
    }
    if let Some(section) = &patch.harness {
        sections.push(Section::Harness);
        harness(section, &mut next.harness, &mut errors);
        custom_roles_need_a_command(&next.harness, &mut errors);
    }
    if let Some(section) = &patch.projects {
        sections.push(Section::Projects);
        projects(section, &mut next.projects, projects_running, &mut errors);
    }
    if let Some(section) = &patch.budgets {
        sections.push(Section::Budgets);
        budgets(section, &mut next.budgets, &mut errors);
    }
    if let Some(section) = &patch.look {
        sections.push(Section::Look);
        look(section, &mut next.look, &mut errors);
    }
    if let Some(section) = &patch.notifications {
        sections.push(Section::Notifications);
        notifications(section, &mut next.notifications);
    }
    if errors.is_empty() {
        Ok(Changes {
            next,
            models,
            sections,
        })
    } else {
        Err(errors)
    }
}

/// A failure at `path`, for the rule `why`.
fn field(path: &str, why: &str) -> FieldError {
    FieldError {
        path: path.to_owned(),
        why: why.to_owned(),
    }
}

/// The model part of a change. A key must name a provider the setup page offers, and carry one
/// key or a clear, never both. A role pick must name a provider the page offers, and a role that
/// needs Meta may only pick Meta.
fn models_change(section: &ModelsPatch, errors: &mut Vec<FieldError>) -> ModelsChange {
    let mut change = ModelsChange::default();
    for (provider, key) in &section.keys {
        if !is_provider(provider) {
            errors.push(field(
                "models.keys",
                "must name a provider the setup page offers",
            ));
            continue;
        }
        let path = format!("models.keys.{provider}");
        match (&key.key, key.clear) {
            (Some(text), None) => match key_text(text) {
                Some(key) => change.keys.push((provider.clone(), KeyChange::Set(key))),
                None => errors.push(field(
                    &path,
                    "must be a key of at most 512 bytes, with no space or control character",
                )),
            },
            (None, Some(true)) => change.keys.push((provider.clone(), KeyChange::Clear)),
            _ => errors.push(field(
                &path,
                "must give a key, or clear set to true, and not both",
            )),
        }
    }
    for (name, pick) in &section.roles {
        let Ok(role) = name.parse::<Role>() else {
            errors.push(field("models.roles", "must name a role the server has"));
            continue;
        };
        let path = format!("models.roles.{role}");
        match pick {
            None => change.roles.push((role, None)),
            Some(provider) if !is_provider(provider) => errors.push(field(
                &path,
                "must be a provider the setup page offers, or null",
            )),
            Some(provider) if role.needs_meta() && provider != MUSE_SPARK_PROVIDER => {
                errors.push(field(&path, "needs a Meta provider"));
            }
            Some(provider) => change.roles.push((role, Some(provider.clone()))),
        }
    }
    change
}

/// The key a change sets, trimmed, or `None` when the provider could not take it. The rule is the
/// one acan-llm applies when it saves a key: one line, with no space or control character, at most
/// [`MAX_KEY_BYTES`] bytes.
fn key_text(raw: &str) -> Option<String> {
    let key = raw.trim();
    let usable = !key.is_empty()
        && key.len() <= MAX_KEY_BYTES
        && !key.chars().any(|c| c.is_whitespace() || c.is_control());
    usable.then(|| key.to_owned())
}

/// The harness part of a change: the adapter of each role, and the custom command.
fn harness(section: &HarnessPatch, next: &mut Harness, errors: &mut Vec<FieldError>) {
    if let Some(roles) = &section.roles {
        for (name, adapter) in roles {
            let Ok(role) = name.parse::<Role>() else {
                errors.push(field("harness.roles", "must name a role the server has"));
                continue;
            };
            let path = format!("harness.roles.{role}");
            match Adapter::parse(adapter) {
                Some(adapter) => {
                    next.roles.insert(role, adapter);
                }
                None => errors.push(field(
                    &path,
                    &format!("must be one of {}", Adapter::NAMES.join(", ")),
                )),
            }
        }
    }
    if let Some(command) = &section.custom_command {
        let problems = command_problems(command);
        if problems.is_empty() {
            next.custom_command = command.clone();
        }
        for problem in problems {
            errors.push(field("harness.custom_command", &problem));
        }
    }
}

/// The rules a custom command breaks, one message for each rule broken. An empty command is valid
/// and clears the command.
fn command_problems(args: &[String]) -> Vec<String> {
    let mut problems = Vec::new();
    if args.len() > MAX_COMMAND_ARGS {
        problems.push(format!("must have at most {MAX_COMMAND_ARGS} arguments"));
    }
    if args.iter().any(String::is_empty) {
        problems.push("no argument may be empty".to_owned());
    }
    if args.iter().any(|arg| arg.len() > MAX_ARG_BYTES) {
        problems.push(format!(
            "each argument must be at most {MAX_ARG_BYTES} bytes"
        ));
    }
    if args.iter().any(|arg| arg.contains('\0')) {
        problems.push("no argument may contain a NUL character".to_owned());
    }
    problems
}

/// The custom adapter needs a command. This runs on the harness as it would stand after the change,
/// so a command already stored counts, and a change that clears it is refused.
fn custom_roles_need_a_command(harness: &Harness, errors: &mut Vec<FieldError>) {
    if !harness.custom_command.is_empty() {
        return;
    }
    for (role, adapter) in &harness.roles {
        if *adapter == Adapter::Custom {
            errors.push(field(
                &format!("harness.roles.{role}"),
                "the custom adapter needs a custom_command",
            ));
        }
    }
}

/// Whether `path` is an absolute path to a folder that exists.
fn is_folder(path: &str) -> bool {
    let path = Path::new(path);
    path.is_absolute() && path.is_dir()
}

/// The projects part of a change. Adds, removes and worker counts are recorded for the next start.
/// A statemap LLM flag names a project the server runs, and takes effect at once.
fn projects(
    section: &ProjectsPatch,
    next: &mut Projects,
    projects_running: &[&str],
    errors: &mut Vec<FieldError>,
) {
    if let Some(paths) = &section.add {
        for path in paths {
            if !is_folder(path) {
                errors.push(field(
                    "projects.add",
                    "must be absolute paths to folders that exist",
                ));
            } else if !next.add.contains(path) {
                next.add.push(path.clone());
            }
        }
    }
    if let Some(ids) = &section.remove {
        for id in ids {
            if !valid_id(id) {
                errors.push(field("projects.remove", "must be project ids"));
            } else if !next.remove.contains(id) {
                next.remove.push(id.clone());
            }
        }
    }
    if let Some(workers) = &section.workers {
        for (id, count) in workers {
            if !valid_id(id) {
                errors.push(field("projects.workers", "must name project ids"));
                continue;
            }
            let path = format!("projects.workers.{id}");
            match u8::try_from(*count)
                .ok()
                .filter(|count| *count <= MAX_WORKERS)
            {
                Some(count) => {
                    next.workers.insert(id.clone(), count);
                }
                None => errors.push(field(&path, &format!("must be from 0 to {MAX_WORKERS}"))),
            }
        }
    }
    if let Some(flags) = &section.statemap_llm {
        for (id, on) in flags {
            if !valid_id(id) {
                errors.push(field("projects.statemap_llm", "must name project ids"));
            } else if !projects_running.contains(&id.as_str()) {
                let path = format!("projects.statemap_llm.{id}");
                errors.push(field(&path, "must name a project the server runs"));
            } else {
                next.statemap_llm.insert(id.clone(), *on);
            }
        }
    }
}

/// The value of `value` as `T` when it is from `min` to `max`, both included. Otherwise it records
/// the failure at `path`, and the caller keeps the value it had.
fn bounded<T: TryFrom<i64>>(
    path: &str,
    value: Option<i64>,
    min: i64,
    max: i64,
    errors: &mut Vec<FieldError>,
) -> Option<T> {
    let value = value?;
    if (min..=max).contains(&value)
        && let Ok(typed) = T::try_from(value)
    {
        return Some(typed);
    }
    errors.push(field(path, &format!("must be from {min} to {max}")));
    None
}

/// The budgets part of a change. Each bound is inclusive, and the repair rounds stop at the most
/// the order model allows.
fn budgets(section: &BudgetsPatch, next: &mut Budgets, errors: &mut Vec<FieldError>) {
    if let Some(value) = bounded(
        "budgets.repair_rounds",
        section.repair_rounds,
        0,
        i64::from(RepairRounds::MAX),
        errors,
    ) {
        next.repair_rounds = value;
    }
    if let Some(value) = bounded("budgets.max_episodes", section.max_episodes, 1, 16, errors) {
        next.max_episodes = value;
    }
    if let Some(value) = bounded(
        "budgets.wall_time_ms",
        section.wall_time_ms,
        60_000,
        3_600_000,
        errors,
    ) {
        next.wall_time_ms = value;
    }
    if let Some(value) = bounded(
        "budgets.model_cost_micros",
        section.model_cost_micros,
        0,
        50_000_000,
        errors,
    ) {
        next.model_cost_micros = value;
    }
    if let Some(limits) = &section.call_limits {
        for role in Role::ALL {
            let path = format!("budgets.call_limits.{role}");
            if let Some(limit) = bounded(&path, limits.get(role), 1, 500, errors) {
                next.call_limits.set(role, limit);
            }
        }
    }
    if let Some(value) = bounded(
        "budgets.stuck_declare_ms",
        section.stuck_declare_ms,
        60_000,
        3_600_000,
        errors,
    ) {
        next.stuck_declare_ms = value;
    }
    if let Some(value) = bounded(
        "budgets.stuck_build_ms",
        section.stuck_build_ms,
        60_000,
        7_200_000,
        errors,
    ) {
        next.stuck_build_ms = value;
    }
    if let Some(value) = bounded(
        "budgets.drift_threshold",
        section.drift_threshold,
        1,
        100,
        errors,
    ) {
        next.drift_threshold = value;
    }
}

/// The look part of a change. A theme and a label density must be one of the names the page knows.
fn look(section: &LookPatch, next: &mut Look, errors: &mut Vec<FieldError>) {
    if let Some(name) = &section.theme {
        match Theme::parse(name) {
            Some(theme) => next.theme = theme,
            None => errors.push(field(
                "look.theme",
                &format!("must be one of {}", Theme::NAMES.join(", ")),
            )),
        }
    }
    if let Some(name) = &section.label_density {
        match LabelDensity::parse(name) {
            Some(density) => next.label_density = density,
            None => errors.push(field(
                "look.label_density",
                &format!("must be one of {}", LabelDensity::NAMES.join(", ")),
            )),
        }
    }
    if let Some(value) = section.top_down_default {
        next.top_down_default = value;
    }
    if let Some(value) = section.motion {
        next.motion = value;
    }
    if let Some(value) = section.idle_bob {
        next.idle_bob = value;
    }
    if let Some(value) = section.reduced_motion {
        next.reduced_motion = value;
    }
}

/// The notifications part of a change. Each is a flag, so any value is one the server takes.
fn notifications(section: &NotificationsPatch, next: &mut Notifications) {
    if let Some(value) = section.needs_you {
        next.needs_you = value;
    }
    if let Some(value) = section.sound {
        next.sound = value;
    }
    if let Some(value) = section.browser {
        next.browser = value;
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    /// The projects the server runs, for the checks that name one.
    const RUNNING: &[&str] = &["acan-wip"];

    fn patch(value: Value) -> Patch {
        serde_json::from_value(value).expect("the patch parses")
    }

    fn accepted(value: Value) -> Changes {
        check(&patch(value), &Stored::default(), RUNNING).expect("the change is accepted")
    }

    fn refused(value: Value) -> Vec<FieldError> {
        check(&patch(value), &Stored::default(), RUNNING)
            .err()
            .expect("the change is refused")
    }

    fn names(errors: &[FieldError], path: &str) -> bool {
        errors.iter().any(|error| error.path == path)
    }

    fn models_of(value: Value) -> ModelsChange {
        accepted(value).models.expect("the change names models")
    }

    #[test]
    fn repair_rounds_accept_both_ends_and_refuse_outside_them() {
        accepted(json!({"budgets": {"repair_rounds": 0}}));
        accepted(json!({"budgets": {"repair_rounds": 8}}));
        let high = refused(json!({"budgets": {"repair_rounds": 9}}));
        assert_eq!(high.len(), 1);
        assert_eq!(high[0].path, "budgets.repair_rounds");
        assert_eq!(high[0].why, "must be from 0 to 8");
        assert!(names(
            &refused(json!({"budgets": {"repair_rounds": -1}})),
            "budgets.repair_rounds"
        ));
    }

    #[test]
    fn each_budget_has_its_own_inclusive_range() {
        let ranges = [
            ("max_episodes", 1, 16),
            ("wall_time_ms", 60_000, 3_600_000),
            ("model_cost_micros", 0, 50_000_000),
            ("stuck_declare_ms", 60_000, 3_600_000),
            ("stuck_build_ms", 60_000, 7_200_000),
            ("drift_threshold", 1, 100),
        ];
        for (field, low, high) in ranges {
            let path = format!("budgets.{field}");
            accepted(json!({ "budgets": { (field): low } }));
            accepted(json!({ "budgets": { (field): high } }));
            assert!(
                names(&refused(json!({ "budgets": { (field): low - 1 } })), &path),
                "{path} low"
            );
            assert!(
                names(&refused(json!({ "budgets": { (field): high + 1 } })), &path),
                "{path} high"
            );
        }
    }

    #[test]
    fn each_call_limit_is_from_one_to_five_hundred() {
        accepted(json!({"budgets": {"call_limits": {"foreman": 1, "wonder": 500}}}));
        assert!(names(
            &refused(json!({"budgets": {"call_limits": {"foreman": 0}}})),
            "budgets.call_limits.foreman"
        ));
        assert!(names(
            &refused(json!({"budgets": {"call_limits": {"worker": 501}}})),
            "budgets.call_limits.worker"
        ));
    }

    #[test]
    fn the_statemap_role_has_a_call_limit_like_the_others() {
        let limits = accepted(json!({"budgets": {"call_limits": {"statemap": 7}}}))
            .next
            .budgets
            .call_limits;
        assert_eq!(limits.statemap, 7);
        assert_eq!(limits.foreman, 30, "the other limits keep their defaults");
        assert!(names(
            &refused(json!({"budgets": {"call_limits": {"statemap": 501}}})),
            "budgets.call_limits.statemap"
        ));
    }

    #[test]
    fn statemap_llm_flags_name_running_projects_and_hold_a_bool() {
        let on = accepted(json!({"projects": {"statemap_llm": {"acan-wip": true}}}));
        assert_eq!(on.next.projects.statemap_llm.get("acan-wip"), Some(&true));
        assert_eq!(on.sections, vec![Section::Projects]);
        let flag = patch(json!({"projects": {"statemap_llm": {"acan-wip": true}}}));
        assert!(
            !flag.projects.as_ref().expect("the section").needs_restart(),
            "a flag alone needs no restart"
        );
        assert!(names(
            &refused(json!({"projects": {"statemap_llm": {"no-such-project": true}}})),
            "projects.statemap_llm.no-such-project"
        ));
        assert!(names(
            &refused(json!({"projects": {"statemap_llm": {"Bad Id": true}}})),
            "projects.statemap_llm"
        ));
        assert!(
            serde_json::from_value::<Patch>(
                json!({"projects": {"statemap_llm": {"acan-wip": "yes"}}})
            )
            .is_err(),
            "a flag that is not a bool does not parse"
        );
    }

    #[test]
    fn the_patch_refuses_the_read_only_and_unknown_fields() {
        for value in [
            json!({"tokens": {}}),
            json!({"about": {}}),
            json!({"version": 1}),
            json!({"budgets": {"nope": 1}}),
            json!({"look": {"theme": "paper", "extra": true}}),
            json!({"models": {"keys": {"meta": {"key": "a", "extra": 1}}}}),
        ] {
            assert!(serde_json::from_value::<Patch>(value).is_err());
        }
    }

    #[test]
    fn a_change_names_only_the_sections_it_sets_in_their_fixed_order() {
        let changes = accepted(json!({
            "look": {"theme": "dark"},
            "budgets": {"drift_threshold": 10},
        }));
        assert_eq!(changes.sections, vec![Section::Budgets, Section::Look]);
    }

    #[test]
    fn a_custom_role_needs_a_command_in_the_change_or_already_stored() {
        assert!(names(
            &refused(json!({"harness": {"roles": {"foreman": "custom"}}})),
            "harness.roles.foreman"
        ));
        accepted(json!({
            "harness": {"roles": {"foreman": "custom"}, "custom_command": ["run", "it"]}
        }));

        let mut stored = Stored::default();
        stored.harness.roles.insert(Role::Worker, Adapter::Custom);
        stored.harness.custom_command = vec!["run".to_owned()];
        assert!(
            check(
                &patch(json!({"harness": {"roles": {"fast": "builtin"}}})),
                &stored,
                RUNNING
            )
            .is_ok()
        );
        let cleared = check(
            &patch(json!({"harness": {"custom_command": []}})),
            &stored,
            RUNNING,
        )
        .err()
        .expect("clearing the command leaves a custom role without one");
        assert!(names(&cleared, "harness.roles.worker"));
    }

    #[test]
    fn custom_command_limits_hold_at_their_edges() {
        let args: Vec<String> = (0..64).map(|index| index.to_string()).collect();
        accepted(json!({"harness": {"custom_command": args}}));
        let too_many: Vec<String> = (0..65).map(|index| index.to_string()).collect();
        assert!(names(
            &refused(json!({"harness": {"custom_command": too_many}})),
            "harness.custom_command"
        ));
        accepted(json!({"harness": {"custom_command": ["x".repeat(512)]}}));
        assert!(names(
            &refused(json!({"harness": {"custom_command": ["x".repeat(513)]}})),
            "harness.custom_command"
        ));
        assert!(names(
            &refused(json!({"harness": {"custom_command": [""]}})),
            "harness.custom_command"
        ));
        assert!(names(
            &refused(json!({"harness": {"custom_command": ["a\u{0}b"]}})),
            "harness.custom_command"
        ));
        let cleared = accepted(json!({"harness": {"custom_command": []}}));
        assert!(cleared.next.harness.custom_command.is_empty());
    }

    #[test]
    fn an_unknown_adapter_or_role_is_refused() {
        assert!(names(
            &refused(json!({"harness": {"roles": {"foreman": "gpt"}}})),
            "harness.roles.foreman"
        ));
        assert!(names(
            &refused(json!({"harness": {"roles": {"judge": "builtin"}}})),
            "harness.roles"
        ));
    }

    #[test]
    fn a_key_names_a_provider_the_page_offers() {
        assert!(names(
            &refused(json!({"models": {"keys": {"openai": {"key": "fake-key-abc"}}}})),
            "models.keys"
        ));
    }

    #[test]
    fn a_refused_key_is_never_echoed_in_its_failure() {
        let errors = refused(json!({"models": {"keys": {"openai": {"key": "SECRET-VALUE"}}}}));
        let text = serde_json::to_string(&errors.iter().map(|e| &e.why).collect::<Vec<_>>())
            .expect("the failures serialize");
        assert!(!text.contains("SECRET"));
        let shape = refused(json!({"models": {"keys": {"meta": {"key": "two words"}}}}));
        assert!(names(&shape, "models.keys.meta"));
    }

    #[test]
    fn a_key_is_one_line_of_at_most_512_bytes_and_is_trimmed() {
        let key = models_of(json!({"models": {"keys": {"meta": {"key": "  abc  "}}}}));
        assert!(matches!(&key.keys[0].1, KeyChange::Set(text) if text == "abc"));
        assert!(names(
            &refused(json!({"models": {"keys": {"meta": {"key": ""}}}})),
            "models.keys.meta"
        ));
        assert!(names(
            &refused(json!({"models": {"keys": {"meta": {"key": "x".repeat(513)}}}})),
            "models.keys.meta"
        ));
    }

    #[test]
    fn a_key_or_a_clear_is_given_but_not_both() {
        let clear = models_of(json!({"models": {"keys": {"meta": {"clear": true}}}}));
        assert!(matches!(clear.keys[0].1, KeyChange::Clear));
        assert!(names(
            &refused(json!({"models": {"keys": {"meta": {"key": "abc", "clear": true}}}})),
            "models.keys.meta"
        ));
        assert!(names(
            &refused(json!({"models": {"keys": {"meta": {}}}})),
            "models.keys.meta"
        ));
        assert!(names(
            &refused(json!({"models": {"keys": {"meta": {"clear": false}}}})),
            "models.keys.meta"
        ));
    }

    #[test]
    fn a_role_picks_an_offered_provider_or_clears_its_pick() {
        let picked = models_of(json!({"models": {"roles": {"foreman": "baseten"}}}));
        assert_eq!(
            picked.roles,
            vec![(Role::Foreman, Some("baseten".to_owned()))]
        );
        let cleared = models_of(json!({"models": {"roles": {"foreman": null}}}));
        assert_eq!(cleared.roles, vec![(Role::Foreman, None)]);
        accepted(json!({"models": {"roles": {"vision": "meta"}}}));
    }

    #[test]
    fn a_role_that_needs_meta_may_only_pick_meta() {
        assert!(names(
            &refused(json!({"models": {"roles": {"vision": "baseten"}}})),
            "models.roles.vision"
        ));
        assert!(names(
            &refused(json!({"models": {"roles": {"foreman": "openai"}}})),
            "models.roles.foreman"
        ));
        assert!(names(
            &refused(json!({"models": {"roles": {"judge": "meta"}}})),
            "models.roles"
        ));
    }

    #[test]
    fn the_look_names_come_from_their_lists() {
        accepted(json!({"look": {"theme": "high_contrast", "label_density": "dense"}}));
        assert!(names(
            &refused(json!({"look": {"theme": "neon"}})),
            "look.theme"
        ));
        assert!(names(
            &refused(json!({"look": {"label_density": "huge"}})),
            "look.label_density"
        ));
    }

    #[test]
    fn project_paths_must_be_absolute_existing_folders() {
        assert!(names(
            &refused(json!({"projects": {"add": ["relative/path"]}})),
            "projects.add"
        ));
        let root = tempfile::tempdir().expect("temp dir");
        let missing = root.path().join("missing");
        assert!(names(
            &refused(json!({"projects": {"add": [missing.display().to_string()]}})),
            "projects.add"
        ));
        let folder = root.path().display().to_string();
        let added = accepted(json!({"projects": {"add": [folder.clone()]}}));
        assert_eq!(added.next.projects.add, vec![folder]);
    }

    #[test]
    fn project_ids_and_worker_counts_are_bounded() {
        assert!(names(
            &refused(json!({"projects": {"remove": ["Bad_Id"]}})),
            "projects.remove"
        ));
        let kept = accepted(json!({"projects": {"remove": ["acan-wip"]}}));
        assert_eq!(kept.next.projects.remove, vec!["acan-wip".to_owned()]);
        let eight = accepted(json!({"projects": {"workers": {"acan-wip": 8}}}));
        assert_eq!(eight.next.projects.workers.get("acan-wip"), Some(&8));
        assert!(names(
            &refused(json!({"projects": {"workers": {"acan-wip": 9}}})),
            "projects.workers.acan-wip"
        ));
        assert!(names(
            &refused(json!({"projects": {"workers": {"acan-wip": -1}}})),
            "projects.workers.acan-wip"
        ));
        assert!(names(
            &refused(json!({"projects": {"workers": {"Bad Id": 1}}})),
            "projects.workers"
        ));
    }

    #[test]
    fn every_failure_in_a_change_is_reported_together() {
        let errors = refused(json!({
            "budgets": {"repair_rounds": 99, "max_episodes": 0},
            "look": {"theme": "neon"},
        }));
        assert_eq!(errors.len(), 3);
    }
}
