//! Where a node app's installed `node_modules` comes from.
//!
//! On Linux a run binds the cache entry's `node_modules` read-only. On other hosts a bind of that
//! tree reads so slowly that the app misses its readiness deadline: Docker Desktop on Windows took
//! about two minutes to load an app that took four seconds from a volume. So there, each
//! lockfile's `node_modules` is copied once into a named volume, and runs mount that volume
//! read-only. The copy is made by `docker cp` from the host, which is much faster than a copy that
//! reads the tree through a bind mount. A finished copy carries a mark file at its root, and a
//! volume without the mark is copied again.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Output, Stdio};
use std::sync::OnceLock;

use tokio::process::Command;
use tokio::sync::Mutex;

/// Whether this host mounts the dependencies from a volume rather than binding the cache entry.
pub(crate) const FROM_VOLUME: bool = cfg!(not(target_os = "linux"));

/// The prefix of every volume this runner creates for dependencies.
const PREFIX: &str = "acan-deps-";
/// The file at a volume's root that marks a finished copy.
const MARK: &str = ".acan-populated";
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

/// The script the helper runs once the copy is in. The copy becomes the run user's, and the mark
/// is written last, so a copy that stopped part way has no mark.
fn finish_script() -> String {
    format!("chown -R {RUN_OWNER} {COPY} && touch {COPY}/{MARK}")
}

/// The arguments that create the helper container for a copy into `volume`. It mounts the volume
/// at [`COPY`], and it runs the finishing script when it is started, which is after the copy.
fn helper_create_args(image: &str, volume: &str) -> Vec<String> {
    let mount = format!("type=volume,source={volume},target={COPY}");
    let script = finish_script();
    let args = [
        "create",
        "--pull",
        "never",
        "--network",
        "none",
        "--entrypoint",
        "sh",
        "--mount",
        mount.as_str(),
        image,
        "-c",
        script.as_str(),
    ];
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

/// The arguments that copy the cache entry's `modules` into the helper container `id`. Docker
/// reads the files from the host, so the copy does not go through a bind mount.
///
/// # Errors
///
/// Returns the refusal when the entry's path is not UTF-8.
fn copy_args(id: &str, modules: &Path) -> Result<Vec<String>, String> {
    let source = modules.join(".");
    let text = source
        .to_str()
        .ok_or_else(|| format!("the dependencies path is not UTF-8: {}", modules.display()))?;
    Ok(vec![
        "cp".to_owned(),
        text.to_owned(),
        format!("{id}:{COPY}"),
    ])
}

/// The arguments that start the helper `id`. It finishes the copy and exits.
fn start_args(id: &str) -> Vec<String> {
    vec!["start".to_owned(), "--attach".to_owned(), id.to_owned()]
}

/// The arguments that remove the helper container `id`.
fn remove_container_args(id: &str) -> Vec<String> {
    vec!["rm".to_owned(), "--force".to_owned(), id.to_owned()]
}

/// The arguments that remove `volume`.
fn remove_args(volume: &str) -> Vec<String> {
    vec!["volume".to_owned(), "rm".to_owned(), volume.to_owned()]
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

/// Replaces the contents of `volume` with a copy of `modules`. A helper container mounts the
/// volume, takes the copy from the host, and then finishes it. A copy that fails is removed, so
/// the next run starts from nothing.
async fn populate(program: &Path, image: &str, modules: &Path, volume: &str) -> Result<(), String> {
    // The volume may hold an earlier copy without its mark. Removing it fails when the volume is
    // absent, which is the usual case, so that result is not read.
    let _ = docker(program, &remove_args(volume)).await;
    let created = docker(program, &helper_create_args(image, volume)).await?;
    if !created.status.success() {
        return Err(format!(
            "cannot create the dependency copy in {volume}: {}",
            String::from_utf8_lossy(&created.stderr).trim()
        ));
    }
    let id = String::from_utf8_lossy(&created.stdout).trim().to_owned();
    if id.is_empty() {
        return Err(format!("docker named no helper container for {volume}"));
    }
    let copied = copy_and_finish(program, modules, volume, &id).await;
    let _ = docker(program, &remove_container_args(&id)).await;
    if copied.is_err() {
        let _ = docker(program, &remove_args(volume)).await;
    }
    copied
}

/// Copies `modules` into the helper `id`, then starts the helper so that it finishes the copy.
async fn copy_and_finish(
    program: &Path,
    modules: &Path,
    volume: &str,
    id: &str,
) -> Result<(), String> {
    let copied = docker(program, &copy_args(id, modules)?).await?;
    if !copied.status.success() {
        return Err(format!(
            "cannot copy the dependencies into {volume}: {}",
            String::from_utf8_lossy(&copied.stderr).trim()
        ));
    }
    let finished = docker(program, &start_args(id)).await?;
    if !finished.status.success() {
        return Err(format!(
            "cannot finish the copy in {volume}: {}",
            String::from_utf8_lossy(&finished.stderr).trim()
        ));
    }
    Ok(())
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
    fn the_helper_mounts_the_volume_and_finishes_the_copy_when_it_is_started() {
        let args = helper_create_args("node:24@sha256:x", "acan-deps-sha256-ab12");
        assert_eq!(args[0], "create", "{args:?}");
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
                "type=volume,source=acan-deps-sha256-ab12,target=/dst"
            )),
            "{args:?}"
        );
        assert_eq!(
            &args[args.len() - 2..],
            [
                "-c",
                "chown -R 1000:1000 /dst && touch /dst/.acan-populated"
            ]
        );
    }

    #[test]
    fn the_copy_comes_from_the_host_into_the_helper_and_the_helper_is_then_started() {
        let modules = Path::new("cache/sha256-ab12/node_modules");
        let source = modules.join(".");
        let args = copy_args("abc123", modules).expect("the copy's arguments");
        assert_eq!(
            args,
            ["cp", source.to_str().expect("a UTF-8 path"), "abc123:/dst"]
        );
        assert_eq!(start_args("abc123"), ["start", "--attach", "abc123"]);
        assert_eq!(remove_container_args("abc123"), ["rm", "--force", "abc123"]);
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
