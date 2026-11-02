//! Resolves the saved setup to one provider per role.
//!
//! A tester keys any number of providers, one key each. Each role uses a provider that has a key.
//! A role with no saved pick takes the first provider on its preference list that has a key. A
//! role that needs a Meta model is off without a Meta key, and no other provider stands in for it.
//! A role with no provider is off, and its status says why.

use serde::Serialize;

use crate::config::{ConfigError, LlmConfig};
use crate::key::ApiKeys;
use crate::providers::{PROVIDERS, catalog, config_for};
use crate::role::Role;
use crate::setup::SavedSetup;

/// Why a role that needs Meta is off.
const NEEDS_META: &str = "needs a Meta key";

/// Why a role is off when no provider has a key.
const NO_KEY: &str = "no model key is set";

/// Whether a provider has a key, and where the key came from. Never the key itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderStatus {
    /// The provider's name.
    pub name: String,
    /// Whether a key is saved from the setup page.
    pub saved: bool,
    /// Whether the environment or the repository's `.env` sets a key. That key wins over a saved
    /// one.
    pub environment: bool,
}

/// The provider one role uses, or why the role is off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoleStatus {
    /// The role.
    pub role: Role,
    /// The provider serving the role. `None` when the role is off.
    pub provider: Option<String>,
    /// Whether the tester picked this provider. `false` when the default serves the role.
    pub picked: bool,
    /// Why the role is off. `None` when a provider serves it.
    pub off_reason: Option<&'static str>,
}

/// The setup resolved for one server: who has a key, who serves each role, and the keys and
/// routing a live client needs.
#[derive(Debug)]
pub struct Plan {
    /// One entry per provider the setup page offers, in its order.
    pub providers: Vec<ProviderStatus>,
    /// One entry per role, in [`Role::ALL`] order.
    pub roles: Vec<RoleStatus>,
    /// The routing, when every role that may run on any provider has one. A role off for want of
    /// Meta is routed to Meta, so its calls fail. `None` otherwise: calls get the offline replies.
    pub config: Option<LlmConfig>,
    /// The keys. The environment wins over a saved key for the same provider.
    pub keys: ApiKeys,
}

/// The keys the environment and the repository's `.env` set, one per provider.
///
/// # Errors
///
/// Returns the errors of [`catalog`], which cannot happen for the fixed blocks.
pub fn environment_keys() -> Result<ApiKeys, ConfigError> {
    Ok(ApiKeys::from_env(&catalog()?))
}

/// Resolves `saved` against `environment`, the keys from [`environment_keys`].
///
/// # Errors
///
/// Returns the errors of [`config_for`] when the routing cannot be built.
pub fn resolve(saved: &SavedSetup, environment: &ApiKeys) -> Result<Plan, ConfigError> {
    let providers: Vec<ProviderStatus> = PROVIDERS
        .iter()
        .map(|&name| ProviderStatus {
            name: name.to_owned(),
            saved: saved.keys.contains_key(name),
            environment: environment.has(name),
        })
        .collect();
    let available: Vec<&'static str> = PROVIDERS
        .iter()
        .copied()
        .filter(|name| saved.keys.contains_key(*name) || environment.has(name))
        .collect();

    let roles: Vec<RoleStatus> = Role::ALL
        .iter()
        .map(|&role| {
            let pick = saved.roles.get(&role).map(String::as_str);
            choose(role, role.needs_meta(), pick, &available)
        })
        .collect();

    let mut keys = environment.clone();
    for (provider, key) in &saved.keys {
        keys = keys.with_fallback(provider, key.clone());
    }

    // A role that needs Meta and is off is routed to Meta anyway. Its calls then fail for want of
    // a key before any request is sent, so no other provider answers for it.
    let config = if roles
        .iter()
        .all(|role| role.provider.is_some() || role.role.needs_meta())
    {
        let routes: Vec<(Role, &str)> = roles
            .iter()
            .map(|role| (role.role, role.provider.as_deref().unwrap_or("meta")))
            .collect();
        Some(config_for(&routes)?)
    } else {
        None
    };
    Ok(Plan {
        providers,
        roles,
        config,
        keys,
    })
}

/// The providers `role` tries in order when no pick is saved.
fn preference(role: Role, needs_meta: bool) -> &'static [&'static str] {
    if needs_meta {
        return &["meta"];
    }
    match role {
        Role::Foreman => &["meta", "baseten", "fireworks"],
        Role::Worker | Role::Fast => &["baseten", "fireworks", "meta"],
        Role::Vision => &["meta"],
    }
}

/// Picks the provider for one role. `available` lists the providers that have a key. A saved
/// pick holds when its provider has a key and the role may use it. Otherwise the first provider
/// on the role's preference list that has a key serves it.
fn choose(
    role: Role,
    needs_meta: bool,
    pick: Option<&str>,
    available: &[&'static str],
) -> RoleStatus {
    let may_use = |provider: &str| !needs_meta || provider == "meta";
    let picked = pick.and_then(|pick| {
        available
            .iter()
            .copied()
            .find(|provider| *provider == pick && may_use(provider))
    });
    if let Some(provider) = picked {
        return RoleStatus {
            role,
            provider: Some(provider.to_owned()),
            picked: true,
            off_reason: None,
        };
    }
    let default = preference(role, needs_meta)
        .iter()
        .copied()
        .find(|provider| available.contains(provider));
    match default {
        Some(provider) => RoleStatus {
            role,
            provider: Some(provider.to_owned()),
            picked: false,
            off_reason: None,
        },
        None => RoleStatus {
            role,
            provider: None,
            picked: false,
            off_reason: Some(if needs_meta { NEEDS_META } else { NO_KEY }),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::key::ApiKey;

    fn saved(keys: &[&str], roles: &[(Role, &str)]) -> SavedSetup {
        SavedSetup {
            keys: keys
                .iter()
                .map(|provider| ((*provider).to_owned(), ApiKey::new("saved-key-0123456789")))
                .collect(),
            roles: roles
                .iter()
                .map(|(role, provider)| (*role, (*provider).to_owned()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    fn served_by(plan: &Plan) -> Vec<Option<&str>> {
        plan.roles
            .iter()
            .map(|role| role.provider.as_deref())
            .collect()
    }

    #[test]
    fn meta_alone_serves_every_role() {
        let plan = resolve(&saved(&["meta"], &[]), &ApiKeys::default()).expect("resolves");
        assert_eq!(served_by(&plan), [Some("meta"); 4]);
        assert!(plan.config.is_some());
    }

    #[test]
    fn baseten_alone_serves_every_role_but_vision_which_needs_meta() {
        let plan = resolve(&saved(&["baseten"], &[]), &ApiKeys::default()).expect("resolves");
        assert_eq!(
            served_by(&plan),
            [Some("baseten"), Some("baseten"), Some("baseten"), None]
        );
        assert_eq!(plan.roles[3].off_reason, Some(NEEDS_META));
        assert!(plan.config.is_some(), "the other roles still run live");
    }

    #[test]
    fn vision_that_is_off_is_routed_to_meta_so_it_never_reaches_another_provider() {
        let plan = resolve(&saved(&["baseten"], &[]), &ApiKeys::default()).expect("resolves");
        let config = plan.config.expect("the config is built");
        assert_eq!(config.route_of(Role::Vision), "meta");
        assert_eq!(config.route_of(Role::Worker), "baseten");
    }

    #[test]
    fn fireworks_alone_serves_every_role_but_vision_which_needs_meta() {
        let plan = resolve(&saved(&["fireworks"], &[]), &ApiKeys::default()).expect("resolves");
        assert_eq!(
            served_by(&plan),
            [
                Some("fireworks"),
                Some("fireworks"),
                Some("fireworks"),
                None
            ]
        );
    }

    #[test]
    fn with_meta_and_baseten_the_foreman_is_meta_and_the_workers_are_baseten() {
        let plan =
            resolve(&saved(&["meta", "baseten"], &[]), &ApiKeys::default()).expect("resolves");
        assert_eq!(
            served_by(&plan),
            [Some("meta"), Some("baseten"), Some("baseten"), Some("meta")]
        );
        assert!(plan.roles.iter().all(|role| !role.picked));
    }

    #[test]
    fn a_pick_for_a_provider_with_a_key_is_honored() {
        let plan = resolve(
            &saved(&["meta", "baseten"], &[(Role::Worker, "meta")]),
            &ApiKeys::default(),
        )
        .expect("resolves");
        assert_eq!(served_by(&plan)[1], Some("meta"));
        assert!(plan.roles[1].picked);
    }

    #[test]
    fn a_pick_for_a_provider_without_a_key_falls_back_to_the_default() {
        let plan = resolve(
            &saved(&["baseten"], &[(Role::Worker, "fireworks")]),
            &ApiKeys::default(),
        )
        .expect("resolves");
        assert_eq!(served_by(&plan)[1], Some("baseten"));
        assert!(!plan.roles[1].picked, "the stale pick does not count");
    }

    #[test]
    fn an_environment_key_wins_over_a_saved_key_for_the_same_provider() {
        let mut environment = ApiKeys::default();
        environment.insert("baseten", ApiKey::new("environment-key-0123456789"));
        let plan = resolve(&saved(&["baseten"], &[]), &environment).expect("resolves");
        assert_eq!(
            plan.keys.get("baseten").map(ApiKey::secret_text),
            Some("environment-key-0123456789")
        );
        let status = &plan.providers[1];
        assert_eq!(
            (status.name.as_str(), status.saved, status.environment),
            ("baseten", true, true)
        );
    }

    #[test]
    fn with_no_key_every_role_is_off_and_calls_are_offline() {
        let plan = resolve(&saved(&[], &[]), &ApiKeys::default()).expect("resolves");
        assert!(plan.config.is_none());
        for role in &plan.roles {
            assert_eq!(role.provider, None);
            let reason = if role.role.needs_meta() {
                NEEDS_META
            } else {
                NO_KEY
            };
            assert_eq!(role.off_reason, Some(reason));
        }
    }

    #[test]
    fn a_role_that_needs_meta_is_off_without_a_meta_key_and_says_so() {
        let picked = choose(Role::Worker, true, None, &["baseten", "fireworks"]);
        assert_eq!(picked.provider, None);
        assert_eq!(picked.off_reason, Some(NEEDS_META));
    }

    #[test]
    fn a_role_that_needs_meta_takes_meta_when_it_has_a_key() {
        let picked = choose(Role::Worker, true, None, &["baseten", "meta"]);
        assert_eq!(picked.provider.as_deref(), Some("meta"));
        assert_eq!(picked.off_reason, None);
    }

    #[test]
    fn a_role_that_needs_meta_refuses_a_pick_of_another_provider() {
        let picked = choose(Role::Worker, true, Some("baseten"), &["baseten", "meta"]);
        assert_eq!(picked.provider.as_deref(), Some("meta"));
        assert!(!picked.picked);
    }
}
