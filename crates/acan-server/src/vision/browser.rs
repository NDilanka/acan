//! Renders one captured page to a PNG with a headless Chromium browser on the host.
//!
//! The page is written to a fresh temporary directory and opened from `file://`, with every host
//! name unresolvable and a proxy that refuses connections. The browser gets a throwaway profile in
//! the same directory, and the directory is removed after every render.
//!
//! Scripts are turned off by the content security policy [`inert`] puts first in the page, not by
//! `--blink-settings=scriptEnabled=false`: with that flag the installed Edge exits without writing
//! a screenshot, in both headless modes.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};

/// Edge, the default browser.
pub const EDGE: &str = r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe";
/// Chrome, used when Edge is not installed.
pub const CHROME: &str = r"C:\Program Files\Google\Chrome\Application\chrome.exe";
/// The viewport every page is rendered at.
pub(crate) const WIDTH: u32 = 1280;
pub(crate) const HEIGHT: u32 = 800;

/// How long one render may take. A browser still running then is killed.
const RENDER_TIMEOUT: Duration = Duration::from_secs(45);
/// How often removing the render directory is tried. The browser's helper processes can hold
/// profile files for a moment after the main process exits.
const REMOVE_ATTEMPTS: u32 = 20;
const REMOVE_PAUSE: Duration = Duration::from_millis(250);

/// The charset every rendered page declares, first in its head.
const CHARSET: &str = r#"<meta charset="utf-8">"#;

/// The policy that leads every rendered page. It runs no script and blocks every load except
/// inline styles and `data:` images, so the page cannot pull a `file://` resource into the
/// screenshot. [`inert`] renames every `<meta` tag of the page itself except its utf-8 charset
/// declarations, so a candidate's refresh or policy tag does nothing.
const POLICY: &str = r#"<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'none'; style-src 'unsafe-inline'; img-src data:">"#;

/// Characters percent-encoded in a `file://` path.
const PATH: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

/// The browser used when the configuration names none: Edge, then Chrome. When neither is
/// installed this is the Edge path, and every render reports that it is missing.
#[must_use]
pub fn default_browser() -> PathBuf {
    [EDGE, CHROME]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(EDGE))
}

/// Why a page has no screenshot.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RenderError {
    #[error("the browser {} is not installed", .0.display())]
    NoBrowser(PathBuf),
    #[error("the render directory failed: {0}")]
    Io(#[from] io::Error),
    #[error("the browser did not finish within {} seconds", RENDER_TIMEOUT.as_secs())]
    Timeout,
    #[error("the browser exited with {status} and wrote no screenshot")]
    NoScreenshot { status: String },
    #[error("the browser's screenshot is not a PNG")]
    NotPng,
}

/// A rendered page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Screenshot {
    pub(crate) png: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// Renders `html` with `browser` and returns the screenshot. The render directory is removed
/// whether or not the render succeeds.
///
/// # Errors
///
/// Returns [`RenderError`] when the browser is missing, fails, times out or writes no PNG.
pub(crate) async fn render(browser: &Path, html: &str) -> Result<Screenshot, RenderError> {
    if !browser.is_file() {
        return Err(RenderError::NoBrowser(browser.to_path_buf()));
    }
    let dir = RenderDir::create().await?;
    let rendered = render_in(browser, html, dir.path()).await;
    dir.remove().await;
    rendered
}

async fn render_in(browser: &Path, html: &str, dir: &Path) -> Result<Screenshot, RenderError> {
    let page = dir.join("page.html");
    let shot = dir.join("shot.png");
    let profile = dir.join("profile");
    // The candidate's HTML is untrusted: it is only ever opened inert, never as a live page.
    tokio::fs::write(&page, inert(html)).await?;
    let mut command = tokio::process::Command::new(browser);
    command
        .args(arguments(&page, &shot, &profile))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let status = tokio::time::timeout(RENDER_TIMEOUT, command.status())
        .await
        .map_err(|_| RenderError::Timeout)??;
    let png = match tokio::fs::read(&shot).await {
        Ok(png) => png,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(RenderError::NoScreenshot {
                status: status.to_string(),
            });
        }
        Err(err) => return Err(err.into()),
    };
    let (width, height) = png_size(&png).ok_or(RenderError::NotPng)?;
    Ok(Screenshot { png, width, height })
}

/// The browser's command line for one render.
pub(crate) fn arguments(page: &Path, shot: &Path, profile: &Path) -> Vec<OsString> {
    let flag = |name: &str, path: &Path| {
        let mut flag = OsString::from(name);
        flag.push(path);
        flag
    };
    vec![
        "--headless=new".into(),
        format!("--window-size={WIDTH},{HEIGHT}").into(),
        flag("--screenshot=", shot),
        "--host-resolver-rules=MAP * ~NOTFOUND".into(),
        "--proxy-server=127.0.0.1:9".into(),
        "--no-first-run".into(),
        "--disable-extensions".into(),
        flag("--user-data-dir=", profile),
        file_url(page).into(),
    ]
}

/// `html` with every `<meta` tag renamed to an unknown element except the page's own utf-8
/// charset declarations, and [`CHARSET`] and [`POLICY`] first in the head. Tag names are matched
/// without regard to case, as the HTML parser does.
pub(crate) fn inert(html: &str) -> String {
    const META: &str = "<meta";
    let lower = html.to_ascii_lowercase();
    let mut neutral = String::with_capacity(html.len() + CHARSET.len() + POLICY.len());
    let mut from = 0;
    for (at, _) in lower.match_indices(META) {
        neutral.push_str(&html[from..at]);
        if is_utf8_declaration(&lower[at..]) {
            neutral.push_str(&html[at..at + META.len()]);
        } else {
            neutral.push_str("<x-inert-meta");
        }
        from = at + META.len();
    }
    neutral.push_str(&html[from..]);
    // The body is already decoded to a UTF-8 String, so every page is declared UTF-8.
    let head = format!("{CHARSET}{POLICY}");
    // A doctype must stay first, or the page renders in quirks mode. Only HTML's own spaces may
    // precede it: any other character starts the body, so the policy must come before it.
    let lead = neutral.len() - neutral.trim_start_matches(is_html_space).len();
    let doctype = neutral[lead..]
        .get(..9)
        .is_some_and(|start| start.eq_ignore_ascii_case("<!doctype"));
    match neutral[lead..].find('>').filter(|_| doctype) {
        Some(end) => {
            let split = lead + end + 1;
            format!("{}{head}{}", &neutral[..split], &neutral[split..])
        }
        None => format!("{head}{neutral}"),
    }
}

/// Whether the lowercased `<meta` tag that starts `tag` only declares a utf-8 charset. A tag that
/// also names a refresh or a policy is not one, nor is one with a character reference, which
/// could spell either, nor one that never closes, nor one that names another charset.
fn is_utf8_declaration(tag: &str) -> bool {
    meta_tag_len(tag).is_some_and(|len| {
        let tag = &tag[..len];
        !tag.contains("refresh")
            && !tag.contains("content-security-policy")
            && !tag.contains('&')
            && names_only_utf8(tag)
    })
}

/// The spaces HTML's tokenizer skips. Unicode spaces such as U+00A0 are not among them.
fn is_html_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c')
}

/// Whether `tag` names a charset, and every charset it names is utf-8.
fn names_only_utf8(tag: &str) -> bool {
    let mut named = false;
    for (at, _) in tag.match_indices("charset") {
        let value = tag[at + "charset".len()..]
            .trim_start_matches(is_html_space)
            .strip_prefix('=')
            .map(|value| value.trim_start_matches(is_html_space))
            .map(|value| value.strip_prefix(['"', '\'']).unwrap_or(value));
        let Some(rest) = value.and_then(|value| value.strip_prefix("utf-8")) else {
            return false;
        };
        if rest.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return false;
        }
        named = true;
    }
    named
}

/// The length of the `<meta` start tag that begins `tag`, through the `>` the HTML tokenizer
/// closes it at. A `>` inside a quoted attribute value does not close it, so a checked tag is
/// never shorter than the one the browser reads. `None` when the tag name goes on past `meta`
/// or the tag never closes.
fn meta_tag_len(tag: &str) -> Option<usize> {
    #[derive(Clone, Copy)]
    enum State {
        BeforeName,
        Name,
        BeforeValue,
        Quoted(u8),
        Unquoted,
    }
    const NAME_END: usize = "<meta".len();
    let space = |byte: u8| matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c');
    let bytes = tag.as_bytes();
    let after = *bytes.get(NAME_END)?;
    if !(space(after) || after == b'/' || after == b'>') {
        return None;
    }
    let mut state = State::BeforeName;
    for (at, &byte) in bytes.iter().enumerate().skip(NAME_END) {
        state = match state {
            // After a quoted value the tokenizer reads the next attribute, as before a name.
            State::Quoted(quote) if byte == quote => State::BeforeName,
            State::Quoted(_) => state,
            _ if byte == b'>' => return Some(at + 1),
            State::BeforeName if space(byte) || byte == b'/' => State::BeforeName,
            State::BeforeName => State::Name,
            State::Name if byte == b'=' => State::BeforeValue,
            State::Name if byte == b'/' => State::BeforeName,
            // Whitespace after a name keeps it open for an `=`, so it stays `Name`.
            State::Name => State::Name,
            State::BeforeValue if space(byte) => State::BeforeValue,
            State::BeforeValue if byte == b'"' || byte == b'\'' => State::Quoted(byte),
            State::BeforeValue => State::Unquoted,
            State::Unquoted if space(byte) => State::BeforeName,
            State::Unquoted => State::Unquoted,
        };
    }
    None
}

/// The `file://` URL of `path`.
pub(crate) fn file_url(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    let encoded = utf8_percent_encode(&raw, PATH);
    if raw.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    }
}

/// The width and height in a PNG's header, or `None` when `bytes` is not a PNG.
pub(crate) fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if bytes.get(..8)? != SIGNATURE || bytes.get(12..16)? != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
    let height = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);
    Some((width, height))
}

/// A fresh temporary directory for one render. [`RenderDir::remove`] deletes it; dropping it
/// without that, as a cancelled render does, makes one attempt.
struct RenderDir {
    path: PathBuf,
    removed: bool,
}

impl RenderDir {
    async fn create() -> io::Result<Self> {
        let path = std::env::temp_dir().join(format!("acan-vision-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir(&path).await?;
        Ok(Self {
            path,
            removed: false,
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    async fn remove(mut self) {
        for attempt in 1..=REMOVE_ATTEMPTS {
            match tokio::fs::remove_dir_all(&self.path).await {
                Ok(()) => {
                    self.removed = true;
                    return;
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    self.removed = true;
                    return;
                }
                Err(err) if attempt == REMOVE_ATTEMPTS => {
                    tracing::warn!(
                        path = %self.path.display(),
                        error = %err,
                        "a render directory stays behind"
                    );
                }
                Err(_) => tokio::time::sleep(REMOVE_PAUSE).await,
            }
        }
    }
}

impl Drop for RenderDir {
    fn drop(&mut self) {
        if !self.removed && std::fs::remove_dir_all(&self.path).is_err() {
            tracing::warn!(path = %self.path.display(), "a render directory stays behind");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_line_turns_the_network_off() {
        let dir = Path::new(r"C:\Temp\acan vision");
        let args = arguments(
            &dir.join("page.html"),
            &dir.join("shot.png"),
            &dir.join("profile"),
        );
        let args: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        for required in [
            "--headless=new",
            "--window-size=1280,800",
            "--host-resolver-rules=MAP * ~NOTFOUND",
            "--proxy-server=127.0.0.1:9",
            "--no-first-run",
            "--disable-extensions",
        ] {
            assert!(
                args.iter().any(|arg| arg == required),
                "{required} in {args:?}"
            );
        }
        assert!(args.contains(&format!("--screenshot={}", dir.join("shot.png").display())));
        assert!(args.contains(&format!(
            "--user-data-dir={}",
            dir.join("profile").display()
        )));
        assert_eq!(
            args.last().map(String::as_str),
            Some(file_url(&dir.join("page.html")).as_str())
        );
        assert!(
            !args.iter().any(|arg| arg.starts_with("--blink-settings")),
            "that flag stops the screenshot; the page policy turns scripts off"
        );
    }

    #[test]
    fn a_windows_path_with_spaces_becomes_one_file_url() {
        assert_eq!(
            file_url(Path::new(r"C:\Users\A S U S\page.html")),
            "file:///C:/Users/A%20S%20U%20S/page.html"
        );
        assert_eq!(
            file_url(Path::new("/tmp/a#b.html")),
            "file:///tmp/a%23b.html"
        );
    }

    #[test]
    fn a_page_cannot_refresh_or_relax_the_policy() {
        let page = inert(
            "<!DOCTYPE html><html><head><META http-equiv=\"refresh\" content=\"0;url=file:///C:/secret\">\
             <meta http-equiv=Content-Security-Policy content=\"default-src *\"></head><body>hi</body></html>",
        );
        assert!(
            page.starts_with(&format!("<!DOCTYPE html>{CHARSET}{POLICY}")),
            "{page}"
        );
        assert_eq!(
            page.to_ascii_lowercase().matches("<meta").count(),
            2,
            "{page}"
        );
        assert!(
            page.contains("<x-inert-meta http-equiv=\"refresh\""),
            "{page}"
        );
        assert!(page.ends_with("<body>hi</body></html>"));
    }

    #[test]
    fn a_page_without_a_doctype_starts_with_the_charset_and_the_policy() {
        assert_eq!(inert("<p>x</p>"), format!("{CHARSET}{POLICY}<p>x</p>"));
        assert_eq!(inert(""), format!("{CHARSET}{POLICY}"));
    }

    #[test]
    fn the_page_keeps_its_utf8_charset_declarations() {
        let head = "<head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=utf-8\">\
                    <META Charset='UTF-8'></head>";
        assert_eq!(
            inert(&format!("<!doctype html>{head}")),
            format!("<!doctype html>{CHARSET}{POLICY}{head}")
        );
    }

    #[test]
    fn a_charset_other_than_utf8_is_renamed() {
        let page = |head: &str| inert(&format!("<!doctype html>{head}<p>x</p>"));
        assert_eq!(
            page("<meta charset=\"ISO-8859-1\">"),
            format!(
                "<!doctype html>{CHARSET}{POLICY}<x-inert-meta charset=\"ISO-8859-1\"><p>x</p>"
            )
        );
        assert_eq!(
            page("<meta http-equiv=Content-Type content=\"text/html; charset=shift_jis\">"),
            format!(
                "<!doctype html>{CHARSET}{POLICY}\
                 <x-inert-meta http-equiv=Content-Type content=\"text/html; charset=shift_jis\"><p>x</p>"
            )
        );
        assert_eq!(
            page("<meta charset=utf-8x>"),
            format!("<!doctype html>{CHARSET}{POLICY}<x-inert-meta charset=utf-8x><p>x</p>")
        );
    }

    #[test]
    fn a_charset_tag_that_does_anything_else_is_renamed() {
        for tag in [
            "<meta charset=\"utf-8\" http-equiv=\"refresh\" content=\"0;url=file:///C:/secret\">",
            "<meta charset=utf-8 content=\"a>b\" http-equiv=refresh>",
            "<meta charset=utf-8 x='>' http-equiv=refresh>",
            "<meta charset=utf-8 http-equiv=\"&#114;efresh\" content=0>",
            "<meta charset=\"x\" http-equiv=\"Content-Security-Policy\" content=\"default-src *\">",
            "<meta charset=utf-8",
            "<metadata charset=utf-8>",
        ] {
            let page = inert(tag);
            assert!(
                page.starts_with(&format!("{CHARSET}{POLICY}")),
                "{tag}: {page}"
            );
            assert_eq!(
                page.to_ascii_lowercase().matches("<meta").count(),
                2,
                "{tag}: {page}"
            );
        }
    }

    #[test]
    fn a_unicode_space_before_the_doctype_keeps_the_policy_in_the_head() {
        assert_eq!(
            inert("\u{a0}<!doctype html><p>x</p>"),
            format!("{CHARSET}{POLICY}\u{a0}<!doctype html><p>x</p>")
        );
        assert_eq!(
            inert("<!doctype html><meta charset=\u{a0}utf-8><p>x</p>"),
            format!("<!doctype html>{CHARSET}{POLICY}<x-inert-meta charset=\u{a0}utf-8><p>x</p>")
        );
    }

    #[test]
    fn html_spaces_before_the_doctype_keep_it_first() {
        assert_eq!(
            inert("\n\t<!doctype html><p>x</p>"),
            format!("\n\t<!doctype html>{CHARSET}{POLICY}<p>x</p>")
        );
    }

    #[test]
    fn the_png_header_gives_the_size() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        png.extend_from_slice(&1280_u32.to_be_bytes());
        png.extend_from_slice(&800_u32.to_be_bytes());
        assert_eq!(png_size(&png), Some((1280, 800)));
        assert_eq!(png_size(b"GIF89a"), None);
        assert_eq!(png_size(&png[..20]), None);
    }

    #[tokio::test]
    async fn a_missing_browser_is_reported_before_anything_is_written() {
        let missing = Path::new(r"C:\no\such\browser.exe");
        assert!(matches!(
            render(missing, "<p>x</p>").await,
            Err(RenderError::NoBrowser(_))
        ));
    }

    /// Held by every test that runs the installed browser, so one test's render directory is
    /// never counted in another's cleanup check.
    static BROWSER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Runs the installed browser. Skipped when neither Edge nor Chrome is installed.
    #[tokio::test]
    async fn the_installed_browser_renders_a_page_without_its_script_and_leaves_no_directory() {
        const PAGE: &str = "<!doctype html><html><body style=\"background:#c00\"><h1>Totals</h1>";
        let browser = default_browser();
        if !browser.is_file() {
            eprintln!("skipped: no Edge or Chrome at {}", browser.display());
            return;
        }
        let _browser = BROWSER.lock().await;
        let before = render_dirs();
        let plain = render(&browser, &format!("{PAGE}</body></html>"))
            .await
            .expect("the page renders");
        assert_eq!((plain.width, plain.height), (WIDTH, HEIGHT));
        assert_eq!(png_size(&plain.png), Some((WIDTH, HEIGHT)));
        let hostile = render(
            &browser,
            &format!(
                "{PAGE}<script>document.body.style.background='#0c0';\
                 document.querySelector('h1').textContent='SCRIPT RAN'</script></body></html>"
            ),
        )
        .await
        .expect("the scripted page renders");
        // Compared whole, not with assert_eq, so a failure does not print two PNGs.
        assert!(
            hostile.png == plain.png,
            "the script ran: the scripted page renders differently"
        );
        assert_eq!(render_dirs(), before, "the render directories were removed");
    }

    /// Runs the installed browser. Skipped when neither Edge nor Chrome is installed.
    #[tokio::test]
    async fn a_script_does_not_change_the_screenshot() {
        const HEAD: &str = "<!doctype html><html><body><p id=\"said\">Before the script</p>";
        const SCRIPT: &str =
            "<script>document.getElementById('said').textContent='After the script'</script>";
        let browser = default_browser();
        if !browser.is_file() {
            eprintln!("skipped: no Edge or Chrome at {}", browser.display());
            return;
        }
        let _browser = BROWSER.lock().await;
        let before = render_dirs();
        let a = render(&browser, &format!("{HEAD}{SCRIPT}</body></html>"))
            .await
            .expect("the page with the script renders");
        let b = render(&browser, &format!("{HEAD}</body></html>"))
            .await
            .expect("the page without the script renders");
        // Compared whole, not with assert_eq, so a failure does not print two PNGs.
        assert!(
            a.png == b.png,
            "the script ran: the page with it renders differently"
        );
        assert_eq!(render_dirs(), before, "the render directories were removed");
    }

    fn render_dirs() -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(std::env::temp_dir())
            .expect("the temp directory lists")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("acan-vision-"))
            })
            .collect();
        dirs.sort();
        dirs
    }
}
