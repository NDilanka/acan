//! The `docker create` argument list and the mounts of one probe sequence container.
//!
//! Each field of the [`ContainerPolicy`] is applied here or the run is refused: a runner that
//! cannot apply a field never runs with less isolation. The list passes exactly the variables
//! the policy's `environment` names. The app's port reaches the app through the driver, and
//! nothing is mounted but the worktree, the protected paths from the snapshot, the harness at
//! the policy's `mounts.harness`, and `.git` when the policy keeps it read-only.

use std::num::NonZeroU32;
use std::path::Path;

use acan_intent::{ContainerNetwork, ContainerPolicy, Driver, MissingProtected};

/// The label key every container this runner starts carries. The value is the run id, so
/// cleanup and tests can find the containers of one run.
pub(crate) const LABEL_KEY: &str = "acan.witness";

/// The host paths a container's mounts are built from.
pub(crate) struct MountSources<'a> {
    /// The worktree, mounted read-write at the policy's working directory.
    pub worktree: &'a Path,
    /// The protected-head snapshot the protected paths are mounted from, read-only.
    pub snapshot: &'a Path,
    /// The directory holding the harness, mounted read-only at the policy's harness path.
    pub harness: &'a Path,
    /// An empty directory, mounted over a missing protected path when the policy masks it.
    pub empty: &'a Path,
    /// The install phase's `node_modules` for this run's lockfile, mounted read-only at the
    /// policy's `mounts.dependencies`. Must be present exactly when the policy names that mount.
    pub dependencies: Option<&'a Path>,
}

/// Builds the `--mount` values of a container.
///
/// # Errors
///
/// Returns the refusal reason when a path cannot be written as a mount value, or when the policy
/// refuses a protected path the snapshot does not hold.
pub(crate) fn mount_values(
    policy: &ContainerPolicy,
    sources: &MountSources<'_>,
) -> Result<Vec<String>, String> {
    let workdir = policy.mounts.workdir.as_str();
    let mut mounts = vec![bind(sources.worktree, workdir, false)?];
    let git = sources.worktree.join(".git");
    if policy.mounts.readonly_git && git.exists() {
        mounts.push(bind(&git, &format!("{workdir}/.git"), true)?);
    }
    for protected in &policy.mounts.protected {
        let target = format!("{workdir}/{}", protected.as_str());
        let source = sources.snapshot.join(protected.as_str());
        if source.exists() {
            mounts.push(bind(&source, &target, true)?);
            continue;
        }
        match policy.mounts.missing_protected {
            MissingProtected::Masked => mounts.push(bind(sources.empty, &target, true)?),
            MissingProtected::Refused => {
                return Err(format!(
                    "protected path {} is missing from the snapshot",
                    protected.as_str()
                ));
            }
        }
    }
    mounts.push(bind(sources.harness, policy.mounts.harness.as_str(), true)?);
    match (policy.mounts.dependencies.as_ref(), sources.dependencies) {
        (Some(target), Some(source)) => mounts.push(bind(source, target.as_str(), true)?),
        (None, None) => {}
        (Some(target), None) => {
            return Err(format!(
                "the policy mounts dependencies at {}, but no install was given",
                target.as_str()
            ));
        }
        (None, Some(_)) => {
            return Err("an install was given, but the policy mounts no dependencies".to_owned());
        }
    }
    Ok(mounts)
}

/// The `--mount` value for one bind mount. A comma or quote in a path would split the value,
/// so such a path is refused rather than quoted.
fn bind(source: &Path, target: &str, readonly: bool) -> Result<String, String> {
    let Some(source_text) = source.to_str() else {
        return Err(format!("mount source is not UTF-8: {}", source.display()));
    };
    if [source_text, target]
        .iter()
        .any(|text| text.contains([',', '"']))
    {
        return Err(format!(
            "mount path cannot be passed to docker: {source_text}"
        ));
    }
    let mut value = format!("type=bind,source={source_text},target={target}");
    if readonly {
        value.push_str(",readonly");
    }
    Ok(value)
}

/// The `docker create` arguments of one probe sequence container.
///
/// The container is created first and started separately, so its id is known before it runs and
/// it can always be removed by id. Once started it runs `bun <harness>/driver.ts` with stdin
/// open, where `<harness>` is the policy's harness path: the host writes one JSON line per
/// operation. `--rm` removes the container when it exits.
///
/// # Errors
///
/// Returns the refusal reason when the policy names a variable a container must not get, or an
/// image that is not pinned by digest.
fn shared_args(
    name: &str,
    run_id: &str,
    policy: &ContainerPolicy,
    mounts: &[String],
    interactive: bool,
) -> Result<Vec<String>, String> {
    policy
        .check_environment()
        .map_err(|err| format!("the container policy is refused: {err}"))?;
    if policy.image.digest.is_none() {
        return Err("the image must be pinned by digest".to_owned());
    }
    let memory = policy.memory_bytes.to_string();

    let mut args: Vec<String> = vec!["create".to_owned()];
    if interactive {
        args.push("--interactive".to_owned());
    }
    args.extend(["--rm", "--pull", "never", "--name"].map(str::to_owned));
    args.push(name.to_owned());
    args.push("--label".to_owned());
    args.push(format!("{LABEL_KEY}={run_id}"));
    args.extend(network_args(policy.network));
    args.extend(
        [
            "--read-only",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "--user",
        ]
        .map(str::to_owned),
    );
    args.push(policy.user.to_string());
    args.extend(["--cpus".to_owned(), cpus(policy.cpus_milli)]);
    args.extend(["--memory".to_owned(), memory.clone()]);
    args.extend(["--memory-swap".to_owned(), memory]);
    args.extend(["--pids-limit".to_owned(), policy.pids.to_string()]);
    args.extend([
        "--tmpfs".to_owned(),
        format!("/tmp:rw,nosuid,nodev,size={}", policy.tmp_bytes),
    ]);
    for (key, value) in &policy.environment {
        args.extend(["--env".to_owned(), format!("{key}={value}")]);
    }
    args.extend([
        "--workdir".to_owned(),
        policy.mounts.workdir.as_str().to_owned(),
    ]);
    for mount in mounts {
        args.push("--mount".to_owned());
        args.push(mount.clone());
    }
    Ok(args)
}

/// The image reference a container starts from: the name and the digest the runner checks.
fn image_ref(policy: &ContainerPolicy) -> Result<String, String> {
    let Some(digest) = policy.image.digest.as_ref() else {
        return Err("the image must be pinned by digest".to_owned());
    };
    Ok(format!("{}@{digest}", policy.image.name.as_str()))
}

/// The `docker create` arguments of one probe sequence container.
///
/// The program is the policy's driver: `bun driver.ts` or `node driver.mjs`, in the harness.
///
/// # Errors
///
/// Returns the refusal reason when the policy names a variable a container must not get, an
/// image that is not pinned by digest, or network egress, which probe runs never get.
pub(crate) fn create_args(
    name: &str,
    run_id: &str,
    policy: &ContainerPolicy,
    mounts: &[String],
) -> Result<Vec<String>, String> {
    if matches!(policy.network, ContainerNetwork::Egress {}) {
        return Err("a probe run never gets network egress".to_owned());
    }
    let (program, script) = match policy.driver {
        Driver::Bun => ("bun", "driver.ts"),
        Driver::Node => ("node", "driver.mjs"),
    };
    let mut args = shared_args(name, run_id, policy, mounts, true)?;
    args.extend(["--entrypoint".to_owned(), program.to_owned()]);
    args.push(image_ref(policy)?);
    args.push(format!("{}/{script}", policy.mounts.harness.as_str()));
    Ok(args)
}

/// The `docker create` arguments of one install phase container: `argv[0]` is the entrypoint and
/// the rest its arguments. The scratch directory is mounted read-write at the working directory,
/// and the container has the policy's network egress and nothing else.
///
/// # Errors
///
/// Returns the refusal reason when the policy is not an egress policy, when the argv is empty, or
/// when the policy fails the same checks as a run.
pub(crate) fn install_args(
    name: &str,
    run_id: &str,
    policy: &ContainerPolicy,
    scratch: &Path,
    argv: &[String],
) -> Result<Vec<String>, String> {
    if !matches!(policy.network, ContainerNetwork::Egress {}) {
        return Err("the install phase runs with network egress only".to_owned());
    }
    let Some((program, rest)) = argv.split_first() else {
        return Err("the install argv is empty".to_owned());
    };
    let scratch_mount = bind(scratch, policy.mounts.workdir.as_str(), false)?;
    let mut args = shared_args(name, run_id, policy, &[scratch_mount], false)?;
    args.extend(["--entrypoint".to_owned(), program.clone()]);
    args.push(image_ref(policy)?);
    args.extend(rest.iter().cloned());
    Ok(args)
}

/// The `--network` arguments of the policy. A probe run's policy names `none`: a container that
/// publishes no port has no route to the host or to the rest of the network. The install phase's
/// policy names the Docker bridge, so its containers can fetch packages and nothing else.
fn network_args(network: ContainerNetwork) -> Vec<String> {
    match network {
        ContainerNetwork::None {} => vec!["--network".to_owned(), "none".to_owned()],
        ContainerNetwork::Egress {} => vec!["--network".to_owned(), "bridge".to_owned()],
    }
}

/// The `--cpus` value for a cap in thousandths of a core.
fn cpus(milli: NonZeroU32) -> String {
    format!("{:.3}", f64::from(milli.get()) / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pinned_policy(missing: &str) -> ContainerPolicy {
        let mut policy = ContainerPolicy::default_run();
        policy.mounts.missing_protected =
            serde_json::from_value(serde_json::json!(missing)).expect("a missing-protected rule");
        policy
    }

    #[test]
    fn create_args_are_exact_for_the_pinned_policy() {
        let policy = pinned_policy("refused");
        let mounts = vec![
            "type=bind,source=W,target=/work".to_owned(),
            "type=bind,source=W\\.git,target=/work/.git,readonly".to_owned(),
            "type=bind,source=S\\.acan,target=/work/.acan,readonly".to_owned(),
            "type=bind,source=H,target=/harness,readonly".to_owned(),
        ];
        let args = create_args("acan-witness-R", "R", &policy, &mounts).expect("args build");
        let expected = [
            "create",
            "--interactive",
            "--rm",
            "--pull",
            "never",
            "--name",
            "acan-witness-R",
            "--label",
            "acan.witness=R",
            "--network",
            "none",
            "--read-only",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "--user",
            "1000:1000",
            "--cpus",
            "2.000",
            "--memory",
            "1073741824",
            "--memory-swap",
            "1073741824",
            "--pids-limit",
            "256",
            "--tmpfs",
            "/tmp:rw,nosuid,nodev,size=268435456",
            "--env",
            "HOME=/tmp",
            "--env",
            "PORT=3000",
            "--workdir",
            "/work",
            "--mount",
            "type=bind,source=W,target=/work",
            "--mount",
            "type=bind,source=W\\.git,target=/work/.git,readonly",
            "--mount",
            "type=bind,source=S\\.acan,target=/work/.acan,readonly",
            "--mount",
            "type=bind,source=H,target=/harness,readonly",
            "--entrypoint",
            "bun",
            "oven/bun:1.4.4@sha256:1d22548f177f4ff38e0c15baa3c2c46c6c2060ba2d1bc573bb4720b407548f1d",
            "/harness/driver.ts",
        ];
        assert_eq!(args, expected, "the docker argument list changed");
    }

    #[test]
    fn only_the_variables_the_policy_names_reach_the_container() {
        let mut policy = pinned_policy("refused");
        policy.environment.remove("PORT");
        let args = create_args("n", "r", &policy, &[]).expect("args build");
        let env_values: Vec<&String> = args
            .windows(2)
            .filter(|pair| pair[0] == "--env")
            .map(|pair| &pair[1])
            .collect();
        assert_eq!(env_values, vec!["HOME=/tmp"]);
        assert!(
            !args
                .iter()
                .any(|arg| arg.contains(".env") || arg.contains("KEY") || arg.contains("PORT")),
            "no env file, key or unlisted variable may enter the argument list: {args:?}"
        );
    }

    #[test]
    fn a_credential_variable_in_code_is_refused_before_create() {
        let mut policy = pinned_policy("refused");
        policy
            .environment
            .insert("OPENAI_API_KEY".to_owned(), "sk-test".to_owned());
        let refusal = create_args("n", "r", &policy, &[]).expect_err("a key is refused");
        assert!(refusal.contains("credential"), "{refusal}");
    }

    #[test]
    fn the_harness_is_mounted_and_run_where_the_policy_says() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut policy = pinned_policy("masked");
        policy.mounts.harness = "/probe".parse().expect("a container path");
        let sources = MountSources {
            worktree: dir.path(),
            snapshot: dir.path(),
            harness: dir.path(),
            empty: dir.path(),
            dependencies: None,
        };
        let mounts = mount_values(&policy, &sources).expect("the mounts build");
        let harness = format!(
            "type=bind,source={},target=/probe,readonly",
            dir.path().display()
        );
        assert!(mounts.contains(&harness), "{mounts:?}");
        let args = create_args("n", "r", &policy, &mounts).expect("args build");
        assert_eq!(args.last().map(String::as_str), Some("/probe/driver.ts"));
        assert!(
            !mounts.iter().any(|mount| mount.contains("target=/harness")),
            "the default harness path is not mounted: {mounts:?}"
        );
    }

    #[test]
    fn an_undigested_image_is_refused() {
        let mut policy = pinned_policy("refused");
        policy.image.digest = None;
        let refusal =
            create_args("n", "r", &policy, &[]).expect_err("an unpinned image is refused");
        assert!(refusal.contains("pinned by digest"), "{refusal}");
    }

    #[test]
    fn a_missing_protected_path_is_refused_by_policy() {
        let dir = tempfile::tempdir().expect("temp dir");
        let worktree = dir.path().join("worktree");
        let snapshot = dir.path().join("snapshot");
        let harness = dir.path().join("harness");
        let empty = dir.path().join("empty");
        for path in [&worktree, &snapshot, &harness, &empty] {
            std::fs::create_dir_all(path).expect("dir");
        }
        let sources = MountSources {
            worktree: &worktree,
            snapshot: &snapshot,
            harness: &harness,
            empty: &empty,
            dependencies: None,
        };
        let refusal = mount_values(&pinned_policy("refused"), &sources)
            .expect_err("the snapshot lacks .acan");
        assert!(refusal.contains(".acan"), "{refusal}");
    }

    #[test]
    fn a_missing_protected_path_is_masked_by_an_empty_mount() {
        let dir = tempfile::tempdir().expect("temp dir");
        let worktree = dir.path().join("worktree");
        let snapshot = dir.path().join("snapshot");
        let harness = dir.path().join("harness");
        let empty = dir.path().join("empty");
        for path in [&worktree, &snapshot, &harness, &empty] {
            std::fs::create_dir_all(path).expect("dir");
        }
        let sources = MountSources {
            worktree: &worktree,
            snapshot: &snapshot,
            harness: &harness,
            empty: &empty,
            dependencies: None,
        };
        let mounts = mount_values(&pinned_policy("masked"), &sources).expect("masked mounts");
        let masked = format!(
            "type=bind,source={},target=/work/.acan,readonly",
            empty.display()
        );
        assert!(mounts.contains(&masked), "{mounts:?}");
    }

    #[test]
    fn a_comma_in_a_mount_path_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let odd = dir.path().join("a,b");
        let refusal = bind(&odd, "/work", false).expect_err("comma refused");
        assert!(refusal.contains("cannot be passed"), "{refusal}");
    }
}
