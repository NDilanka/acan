//! Writes and checks the Droid plugin that connects a session to the hook gate.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

const MANIFEST: &str = ".factory-plugin/plugin.json";
const HOOKS: &str = "hooks/hooks.json";
const MCP: &str = ".mcp.json";
const DESCRIPTION: &str = "Connects Droid to the Acan order gate";

/// What the installed plugin should launch and, optionally, which local MCP server to use.
#[derive(Debug, Clone)]
pub struct PluginConfig {
    pub hook_bin: PathBuf,
    pub mcp_url: Option<String>,
    pub version: String,
}

/// The files installed at the supplied plugin root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginBundle {
    pub root: PathBuf,
    pub manifest: PathBuf,
    pub hooks: PathBuf,
    pub mcp: Option<PathBuf>,
}

/// Why a bundle could not be installed or verified.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("hook executable must be an absolute, shell-safe UTF-8 path")]
    BadHookPath,
    #[error("plugin version must be non-blank and contain no control characters")]
    BadVersion,
    #[error("MCP URL must be a local HTTP(S) URL without credentials or a fragment")]
    BadMcpUrl,
    #[error("invalid plugin file {file}: {reason}")]
    InvalidFile {
        file: &'static str,
        reason: &'static str,
    },
    #[error("cannot access plugin path {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
}

/// Installs or replaces each file atomically, without leaving an old MCP configuration behind.
///
/// # Errors
///
/// Returns an error for an invalid config or a path that cannot be created or replaced.
pub fn install_plugin_bundle(
    root: &Path,
    config: &PluginConfig,
) -> Result<PluginBundle, PluginError> {
    let command = hook_command(&config.hook_bin)?;
    if !valid_label(&config.version) {
        return Err(PluginError::BadVersion);
    }
    if let Some(url) = &config.mcp_url {
        validate_mcp_url(url)?;
    }

    let manifest = root.join(MANIFEST);
    let hooks = root.join(HOOKS);
    let mcp = root.join(MCP);
    for dir in [
        root,
        root.join(".factory-plugin").as_path(),
        root.join("hooks").as_path(),
    ] {
        create_private_dir(dir)?;
    }
    write_json(
        &manifest,
        &json!({
            "name": "acan",
            "version": config.version,
            "description": DESCRIPTION,
        }),
    )?;
    write_json(&hooks, &hook_config(&command))?;
    let mcp = if let Some(url) = &config.mcp_url {
        write_json(&mcp, &mcp_config(url))?;
        Some(mcp)
    } else {
        match fs::remove_file(&mcp) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(io_at(&mcp, err)),
        }
        None
    };
    Ok(PluginBundle {
        root: root.to_owned(),
        manifest,
        hooks,
        mcp,
    })
}

/// Checks the manifest, exact hook entries and optional local MCP config without changing them.
///
/// # Errors
///
/// Returns an error for missing required files, invalid JSON/config, or a different executable.
pub fn verify_plugin_bundle(root: &Path, expected_hook_bin: &Path) -> Result<(), PluginError> {
    let command = hook_command(expected_hook_bin)?;
    for dir in [
        root,
        root.join(".factory-plugin").as_path(),
        root.join("hooks").as_path(),
    ] {
        require_dir(dir)?;
    }
    let manifest = read_json(root, MANIFEST, false)?.expect("required manifest");
    let valid_manifest = manifest.as_object().is_some_and(|fields| {
        fields.len() == 3
            && fields.get("name").and_then(Value::as_str) == Some("acan")
            && ["version", "description"].iter().all(|key| {
                fields
                    .get(*key)
                    .and_then(Value::as_str)
                    .is_some_and(valid_label)
            })
    });
    if !valid_manifest {
        return Err(invalid(
            MANIFEST,
            "expected the acan name, version and description",
        ));
    }
    if read_json(root, HOOKS, false)?.as_ref() != Some(&hook_config(&command)) {
        return Err(invalid(
            HOOKS,
            "expected SessionStart and PreToolUse for the supplied hook executable",
        ));
    }
    if let Some(mcp) = read_json(root, MCP, true)? {
        let url = mcp
            .pointer("/mcpServers/acan/url")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid(MCP, "expected a local acan HTTP server"))?;
        if mcp != mcp_config(url) {
            return Err(invalid(MCP, "expected only the local acan HTTP server"));
        }
        validate_mcp_url(url)?;
    }
    Ok(())
}

fn hook_config(command: &str) -> Value {
    json!({ "hooks": {
        "SessionStart": [{
            "matcher": ".*",
            "hooks": [{ "type": "command", "command": command }],
        }],
        "PreToolUse": [{
            "matcher": ".*",
            "hooks": [{ "type": "command", "command": command, "onFailure": "block" }],
        }],
    }})
}

fn mcp_config(url: &str) -> Value {
    json!({ "mcpServers": { "acan": { "type": "http", "url": url } } })
}

fn hook_command(path: &Path) -> Result<String, PluginError> {
    let raw = path.to_str().ok_or(PluginError::BadHookPath)?;
    if !path.is_absolute()
        || path.file_name().is_none()
        || raw.chars().any(|ch| {
            ch.is_control()
                || matches!(
                    ch,
                    '"' | '\''
                        | '$'
                        | '`'
                        | '%'
                        | '!'
                        | ';'
                        | '&'
                        | '|'
                        | '<'
                        | '>'
                        | '('
                        | ')'
                        | '^'
                        | '*'
                        | '?'
                )
        })
    {
        return Err(PluginError::BadHookPath);
    }
    // Droid uses /bin/sh on Unix and cmd.exe on Windows for shell-form hooks. Reject
    // expansion characters before quoting the path as one command, including spaces.
    if cfg!(windows) {
        Ok(format!("\"{raw}\""))
    } else {
        Ok(format!("'{raw}'"))
    }
}

fn valid_label(text: &str) -> bool {
    !text.trim().is_empty() && !text.chars().any(char::is_control)
}

fn validate_mcp_url(raw: &str) -> Result<(), PluginError> {
    let url = reqwest::Url::parse(raw).map_err(|_| PluginError::BadMcpUrl)?;
    if raw.chars().any(|ch| ch.is_whitespace() || ch.is_control())
        || !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !url.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        })
    {
        return Err(PluginError::BadMcpUrl);
    }
    Ok(())
}

fn create_private_dir(path: &Path) -> Result<(), PluginError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(|err| io_at(path, err))?;
    require_dir(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|err| io_at(path, err))?;
    }
    Ok(())
}

fn require_dir(path: &Path) -> Result<(), PluginError> {
    if !fs::symlink_metadata(path)
        .map_err(|err| io_at(path, err))?
        .is_dir()
    {
        return Err(io_at(path, io::Error::other("not a real directory")));
    }
    Ok(())
}

fn write_json(path: &Path, value: &Value) -> Result<(), PluginError> {
    let write = || -> io::Result<()> {
        let dir = path
            .parent()
            .ok_or_else(|| io::Error::other("file has no directory"))?;
        let mut temp = tempfile::NamedTempFile::new_in(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temp.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        serde_json::to_writer_pretty(&mut temp, value).map_err(io::Error::other)?;
        temp.write_all(b"\n")?;
        temp.as_file().sync_all()?;
        temp.persist(path)?;
        Ok(())
    };
    write().map_err(|err| io_at(path, err))
}

fn read_json(
    root: &Path,
    file: &'static str,
    optional: bool,
) -> Result<Option<Value>, PluginError> {
    let path = root.join(file);
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err(invalid(file, "expected a regular file")),
        Err(err) if optional && err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(io_at(&path, err)),
    }
    let raw = fs::read_to_string(&path).map_err(|err| io_at(&path, err))?;
    acan_hook::parse_object(&raw)
        .map(Some)
        .map_err(|_| invalid(file, "expected a JSON object with unique keys"))
}

fn io_at(path: &Path, source: io::Error) -> PluginError {
    PluginError::Io {
        path: path.to_owned(),
        source,
    }
}

fn invalid(file: &'static str, reason: &'static str) -> PluginError {
    PluginError::InvalidFile { file, reason }
}
