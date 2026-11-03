//! Where a node app's installed `node_modules` comes from.
//!
//! On Linux a run binds the cache entry's `node_modules` read-only. On other hosts a bind of that
//! tree reads so slowly that the app misses its readiness deadline: Docker Desktop on Windows took
//! about two minutes to load an app that took four seconds from a volume. So there, each
//! lockfile's `node_modules` is copied once into a named volume, and runs mount that volume
//! read-only. A finished copy carries a mark file at its root, and a volume without the mark is
//! copied again.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Output, Stdio};
use std::sync::OnceLock;

use tokio::process::Command;
use tokio::sync::Mutex;

use crate::docker::bind;

/// Whether this host mounts the dependencies from a volume rather than binding the cache entry.
pub(crate) const FROM_VOLUME: bool = cfg!(not(target_os = "linux"));

/// The prefix of every volume this runner creates for dependencies.
const PREFIX: &str = "acan-deps-";
/// The file at a volume's root that marks a finished copy.
const MARK: &str = ".acan-populated";
/// Where the helper container reads the cache entry's `node_modules`.
const SOURCE: &str = "/src";
/// Where the helper container writes the volume.
const COPY: &str = "/dst";
/// The user a run's app runs as. The copy is owned by it.
const RUN_OWNER: &str = "1000:1000";

/// How a run mounts the install's `node_modules`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DependencyMount<'a> {
    /// The cache entry's `node_modules`, bound from the host.
    Bind(&'a Path),
    /// A named volume that holds a finished copy of it.
    Volume(String),
}

/// The name of the volume that holds the copy of `modules`, the `node_modules` of one cache
/// entry. The entry's name is its lockfile key, so each lockfile has one volume. A character that
/// a volume name cannot hold becomes `-`.
///
/// # Errors
///
/// Returns the refusal when `modules` does not sit in a directory with a name.
pub(crate) fn volume_name(modules: &Path) -> Result<String, String> {
    let key = modules
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            format!(
                "the dependencies at {} do not sit in a named cache entry",
                modules.display()
            )
        })?;
    let safe: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    Ok(format!("{PREFIX}{safe}"))
}

/// The mount for the install's `modules` on this host. A host that binds the cache entry gets the
/// entry itself. Otherwise the volume is made complete first. The first copy for a lockfile can
/// take a minute or more, so the caller does not charge that time to the run's deadline.
///
/// # Errors
///
/// Returns the refusal when the name cannot be made, or when docker cannot make the copy.
pub(crate) async fn prepare<'a>(
    program: &Path,
    image: &str,
    modules: &'a Path,
    from_volume: bool,
) -> Result<DependencyMount<'a>, String> {
    if !from_volume {
        return Ok(DependencyMount::Bind(modules));
    }
    let volume = volume_name(modules)?;
    ensure(program, image, modules, &volume).await?;
    Ok(DependencyMount::Volume(volume))
}

/// Whether a volume must be copied: not when this process has already seen it complete, and not
/// when its mark is there.
pub(crate) fn needs_population(known_complete: bool, marked: bool) -> bool {
    !known_complete && !marked
}

/// The arguments that test whether `volume` holds the mark. The test exits 0 when the mark is
/// there and 1 when it is not.
pub(crate) fn mark_check_args(image: &str, volume: &str) -> Vec<String> {
    let mount = format!("type=volume,source={volume},target={COPY},readonly");
    let mark = format!("{COPY}/{MARK}");
    helper_args("test", &[mount], image, &["-f", mark.as_str()])
}

/// The arguments of the helper that copies the cache entry's `modules` into `volume`. It reads
/// the entry through a read-only mount, makes the copy the run user's, and writes the mark last.
///
/// # Errors
///
/// Returns the refusal when the entry's path cannot be written as a mount.
pub(crate) fn populate_args(
    image: &str,
    modules: &Path,
    volume: &str,
) -> Result<Vec<String>, String> {
    let source = bind(modules, SOURCE, true)?;
    let copy = format!("type=volume,source={volume},target={COPY}");
    let script =
        format!("cp -a {SOURCE}/. {COPY}/ && chown -R {RUN_OWNER} {COPY} && touch {COPY}/{MARK}");
    Ok(helper_args(
        "sh",
        &[source, copy],
        image,
        &["-c", script.as_str()],
    ))
}

/// The arguments that remove `volume`.
fn remove_args(volume: &str) -> Vec<String> {
    vec!["volume".to_owned(), "rm".to_owned(), volume.to_owned()]
}

/// The arguments that create `volume`.
fn create_args(volume: &str) -> Vec<String> {
    vec!["volume".to_owned(), "create".to_owned(), volume.to_owned()]
}

/// The arguments of a short-lived container that runs `entrypoint` with `rest` in `image`. It
/// has no network, and it is removed when it exits.
fn helper_args(entrypoint: &str, mounts: &[String], image: &str, rest: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = [
        "run",
        "--rm",
        "--pull",
        "never",
        "--network",
        "none",
        "--entrypoint",
        entrypoint,
    ]
    .iter()
    .map(|arg| (*arg).to_owned())
    .collect();
    for mount in mounts {
        args.push("--mount".to_owned());
        args.push(mount.clone());
    }
    args.push(image.to_owned());
    args.extend(rest.iter().map(|arg| (*arg).to_owned()));
    args
}

/// The volumes this process has seen complete, by name. A volume in the set is not checked again.
fn known_complete() -> &'static Mutex<BTreeSet<String>> {
    static KNOWN: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    KNOWN.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// Makes `volume` hold a finished copy of `modules`, copying it when it does not. Copies run one
/// at a time, so runs that need the same volume wait for the first copy.
async fn ensure(program: &Path, image: &str, modules: &Path, volume: &str) -> Result<(), String> {
    let mut known = known_complete().lock().await;
    let known_complete = known.contains(volume);
    let marked = !known_complete && mark_present(program, image, volume).await?;
    if needs_population(known_complete, marked) {
        populate(program, image, modules, volume).await?;
    }
    known.insert(volume.to_owned());
    Ok(())
}

/// Whether `volume` holds the mark. A test that fails for any reason other than the mark being
/// absent is an error, so a docker that cannot run is not read as an empty volume.
async fn mark_present(program: &Path, image: &str, volume: &str) -> Result<bool, String> {
    let output = docker(program, &mark_check_args(image, volume)).await?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(format!(
            "cannot check the dependency volume {volume}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

/// Replaces the contents of `volume` with a copy of `modules`. A copy that fails is removed, so
/// the next run starts from nothing.
async fn populate(program: &Path, image: &str, modules: &Path, volume: &str) -> Result<(), String> {
    // The volume may hold an earlier copy without its mark. Removing it fails when the volume is
    // absent, which is the usual case, so that result is not read.
    let _ = docker(program, &remove_args(volume)).await;
    let created = docker(program, &create_args(volume)).await?;
    if !created.status.success() {
        return Err(format!(
            "cannot create the dependency volume {volume}: {}",
            String::from_utf8_lossy(&created.stderr).trim()
        ));
    }
    let copied = docker(program, &populate_args(image, modules, volume)?).await?;
    if copied.status.success() {
        return Ok(());
    }
    let _ = docker(program, &remove_args(volume)).await;
    Err(format!(
        "cannot copy the dependencies into {volume}: {}",
        String::from_utf8_lossy(&copied.stderr).trim()
    ))
}

/// Runs docker with `args` and returns its output. A docker that cannot start is an error.
async fn docker(program: &Path, args: &[String]) -> Result<Output, String> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|err| format!("cannot run {}: {err}", program.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether `name` is a volume name docker accepts: letters and digits to start, then
    /// letters, digits, `_`, `.` or `-`, and at least two characters.
    fn is_docker_volume_name(name: &str) -> bool {
        let mut chars = name.chars();
        name.len() >= 2
            && chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
    }

    #[test]
    fn the_volume_is_named_for_the_cache_entry_key() {
        let modules = Path::new("cache/sha256-ab12/node_modules");
        assert_eq!(
            volume_name(modules).expect("a name"),
            "acan-deps-sha256-ab12"
        );
    }

    #[test]
    fn characters_a_volume_name_cannot_hold_become_dashes() {
        let modules = Path::new("cache/odd key:one/node_modules");
        assert_eq!(
            volume_name(modules).expect("a name"),
            "acan-deps-odd-key-one"
        );
    }

    #[test]
    fn every_volume_name_is_one_docker_accepts() {
        for entry in ["sha256-ab12", "a", "x.y_z-9", "odd key:one", "é"] {
            let modules = Path::new("cache").join(entry).join("node_modules");
            let name = volume_name(&modules).expect("a name");
            assert!(is_docker_volume_name(&name), "{name}");
        }
    }

    #[test]
    fn a_path_without_a_cache_entry_name_is_refused() {
        assert!(volume_name(Path::new("node_modules")).is_err());
        assert!(volume_name(Path::new("/")).is_err());
    }

    #[test]
    fn the_copy_reads_the_entry_read_only_and_writes_the_volume_then_marks_it() {
        let modules = Path::new("cache/sha256-ab12/node_modules");
        let args = populate_args("node:24@sha256:x", modules, "acan-deps-sha256-ab12")
            .expect("the copy's arguments");
        let pairs: Vec<(&str, &str)> = args
            .windows(2)
            .map(|pair| (pair[0].as_str(), pair[1].as_str()))
            .collect();
        assert!(pairs.contains(&("--pull", "never")), "{args:?}");
        assert!(pairs.contains(&("--network", "none")), "{args:?}");
        assert!(pairs.contains(&("--entrypoint", "sh")), "{args:?}");
        assert!(
            pairs.contains(&(
                "--mount",
                "type=bind,source=cache/sha256-ab12/node_modules,target=/src,readonly"
            )),
            "{args:?}"
        );
        assert!(
            pairs.contains(&(
                "--mount",
                "type=volume,source=acan-deps-sha256-ab12,target=/dst"
            )),
            "{args:?}"
        );
        let script = args.last().expect("the script is the last argument");
        assert_eq!(args[args.len() - 2], "-c", "{args:?}");
        let copy = script.find("cp -a /src/.").expect("the copy");
        let owner = script.find("chown -R 1000:1000 /dst").expect("the owner");
        let mark = script.find("touch /dst/.acan-populated").expect("the mark");
        assert!(copy < owner && owner < mark, "{script}");
        assert!(script.ends_with("touch /dst/.acan-populated"), "{script}");
    }

    #[test]
    fn the_mark_check_reads_the_volume_read_only() {
        let args = mark_check_args("node:24@sha256:x", "acan-deps-sha256-ab12");
        assert!(
            args.windows(2).any(|pair| {
                pair[0] == "--mount"
                    && pair[1] == "type=volume,source=acan-deps-sha256-ab12,target=/dst,readonly"
            }),
            "{args:?}"
        );
        assert_eq!(&args[args.len() - 2..], ["-f", "/dst/.acan-populated"]);
    }

    #[test]
    fn a_volume_is_copied_unless_it_is_known_complete_or_marked() {
        assert!(needs_population(false, false));
        assert!(!needs_population(false, true));
        assert!(!needs_population(true, false));
        assert!(!needs_population(true, true));
    }

    #[tokio::test]
    async fn a_host_that_binds_the_entry_gets_the_entry_and_no_copy() {
        let modules = Path::new("cache/sha256-ab12/node_modules");
        let mount = prepare(Path::new("docker"), "image", modules, false)
            .await
            .expect("a bind needs no docker");
        assert_eq!(mount, DependencyMount::Bind(modules));
    }

    #[tokio::test]
    async fn a_volume_for_a_path_without_an_entry_is_refused_before_docker_runs() {
        let refusal = prepare(
            Path::new("acan-no-such-docker"),
            "image",
            Path::new("node_modules"),
            true,
        )
        .await
        .expect_err("the name cannot be made");
        assert!(refusal.contains("named cache entry"), "{refusal}");
    }
}
