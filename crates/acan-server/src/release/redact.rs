//! Removes what must not leave the machine in a bug report: secrets, tokens and private paths.
//!
//! The report is posted on a public issue tracker, so these rules err toward removing text. A
//! secret is the value of an environment variable whose name says it is one. A token is a known
//! shape. A private path is the repository, the home folder, a Windows drive path, or a path under
//! a common user-folder root.

use std::path::Path;

const KEY: &str = "<redacted key>";
const TOKEN: &str = "<redacted token>";
const PATH: &str = "<path>";
const REPO: &str = "<repo>";
const HOME: &str = "~";

/// Environment variables whose names end this way hold secrets.
const SECRET_SUFFIXES: [&str; 4] = ["_API_KEY", "_KEY", "_TOKEN", "_SECRET"];

/// The shortest secret value that is redacted. A shorter value would match ordinary text.
const MIN_SECRET_LEN: usize = 8;

/// The fewest token characters after `acan_`, `sk-` or `Bearer ` that make a token. An owner
/// token is `acan_` and 32 hex digits.
const MIN_TOKEN_LEN: usize = 16;

/// Roots of folders that hold a person's files. A path under one is private.
const USER_ROOTS: [&str; 8] = [
    "/home/",
    "/Users/",
    "/root/",
    "/private/",
    "/var/folders/",
    "/tmp/",
    "/mnt/",
    "/media/",
];

/// What one bug report removes. Built once at startup and kept in the release state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redaction {
    /// Secret values from the environment, longest first.
    pub(super) secrets: Vec<String>,
    /// Path prefixes and what replaces them, longest first.
    pub(super) roots: Vec<(String, &'static str)>,
}

impl Redaction {
    /// Removes nothing but the built-in token shapes and user-folder paths.
    pub const EMPTY: Self = Self {
        secrets: Vec::new(),
        roots: Vec::new(),
    };

    /// The secrets in this process's environment, plus the paths of `repo` and the home folder.
    /// Call it after the `.env` file is loaded, so the keys it sets are included.
    #[must_use]
    pub fn for_repo(repo: &Path) -> Self {
        let mut secrets: Vec<String> = std::env::vars_os()
            .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
            .filter(|(name, value)| is_secret_name(name) && value.len() >= MIN_SECRET_LEN)
            .map(|(_, value)| value)
            .collect();
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        secrets.dedup();

        let mut roots = Vec::new();
        for form in path_forms(repo) {
            roots.push((form, REPO));
        }
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            for form in path_forms(Path::new(&home)) {
                roots.push((form, HOME));
            }
        }
        // A root shorter than this would match ordinary text, so it is never used.
        roots.retain(|(form, _)| form.len() >= 4);
        roots.sort_by_key(|(form, _)| std::cmp::Reverse(form.len()));
        roots.dedup();

        Self { secrets, roots }
    }

    /// Adds one secret value, such as a model key saved while the server runs. A value shorter
    /// than the minimum is not a secret, so it is not added.
    pub fn add_secret(&mut self, secret: &str) {
        if secret.len() < MIN_SECRET_LEN || self.secrets.iter().any(|known| known == secret) {
            return;
        }
        self.secrets.push(secret.to_owned());
        self.secrets
            .sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    }

    /// The text with every secret, token and private path removed. Terminal colour codes are
    /// removed too, so a line reads the same in the report as it did on the terminal.
    #[must_use]
    pub fn scrub(&self, text: &str) -> String {
        let mut text = strip_ansi(text);
        for secret in &self.secrets {
            text = text.replace(secret.as_str(), KEY);
        }
        text = redact_prefixed(&text, "acan_", TOKEN);
        text = redact_prefixed(&text, "sk-", KEY);
        text = redact_prefixed(&text, "Bearer ", "Bearer <redacted token>");
        for (root, replacement) in &self.roots {
            text = text.replace(root.as_str(), replacement);
        }
        redact_private_paths(&text)
    }
}

fn is_secret_name(name: &str) -> bool {
    SECRET_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
}

/// The spellings a path may appear in: as given, absolute, canonical, and with forward slashes.
fn path_forms(path: &Path) -> Vec<String> {
    let mut forms = vec![path.display().to_string()];
    if let Ok(absolute) = std::path::absolute(path) {
        forms.push(absolute.display().to_string());
    }
    if let Ok(canonical) = path.canonicalize() {
        forms.push(canonical.display().to_string());
    }
    let slashed: Vec<String> = forms
        .iter()
        .filter(|form| form.contains('\\'))
        .map(|form| form.replace('\\', "/"))
        .collect();
    forms.extend(slashed);
    forms.sort();
    forms.dedup();
    forms
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')
}

/// Replaces `prefix` and the token characters after it, when at least [`MIN_TOKEN_LEN`] follow
/// and the prefix starts a word. `replacement` stands for the whole match.
fn redact_prefixed(text: &str, prefix: &str, replacement: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(prefix) {
        let (before, from_prefix) = rest.split_at(at);
        out.push_str(before);
        let after = &from_prefix[prefix.len()..];
        let starts_word = before.chars().next_back().is_none_or(|c| !is_token_char(c));
        let run = after
            .find(|c: char| !is_token_char(c))
            .unwrap_or(after.len());
        if starts_word && run >= MIN_TOKEN_LEN {
            out.push_str(replacement);
            rest = &after[run..];
        } else {
            out.push_str(prefix);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Replaces each private path with [`PATH`]. A path starts at a word boundary and is a Windows
/// drive path or a path under one of [`USER_ROOTS`].
fn redact_private_paths(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while at < text.len() {
        let rest = &text[at..];
        let before = text[..at].chars().next_back();
        match private_path_len(rest, before) {
            Some(len) => {
                out.push_str(PATH);
                at += len;
            }
            None => {
                let c = rest.chars().next().expect("at is inside the text");
                out.push(c);
                at += c.len_utf8();
            }
        }
    }
    out
}

fn is_path_boundary(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '\'' | '(' | '[' | '{' | '<' | '=' | ',' | '`')
}

/// The byte length of the private path at the start of `rest`, if one starts there.
fn private_path_len(rest: &str, before: Option<char>) -> Option<usize> {
    if before.is_some_and(|c| !is_path_boundary(c)) {
        return None;
    }
    let mut chars = rest.chars();
    let drive = match (chars.next(), chars.next(), chars.next()) {
        (Some(letter), Some(':'), Some('\\' | '/')) => letter.is_ascii_alphabetic(),
        _ => false,
    };
    if drive {
        // A Windows folder can hold spaces, so the path runs to the end of the line.
        return Some(rest.find(['\r', '\n', '"', '\'']).unwrap_or(rest.len()));
    }
    if USER_ROOTS.iter().any(|root| rest.starts_with(root)) {
        let end = rest
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ')' | ']' | '>' | '`'))
            .unwrap_or(rest.len());
        return Some(end);
    }
    None
}

/// The text without ANSI colour and style sequences.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.clone().next() == Some('[') {
            chars.next();
            // The sequence ends at a final byte between 0x40 and 0x7e.
            for d in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&d) {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_secret(secret: &str) -> Redaction {
        Redaction {
            secrets: vec![secret.to_owned()],
            roots: Vec::new(),
        }
    }

    #[test]
    fn removes_secret_values_wherever_they_appear() {
        let redaction = with_secret("sk-live-provider-secret-value");
        let text = "call failed with key sk-live-provider-secret-value in the body";
        assert_eq!(
            redaction.scrub(text),
            "call failed with key <redacted key> in the body"
        );
    }

    #[test]
    fn a_secret_added_later_is_removed_and_a_short_one_is_not_added() {
        let mut redaction = Redaction::EMPTY;
        redaction.add_secret("meta-saved-key-0123456789");
        redaction.add_secret("short");
        assert_eq!(redaction.secrets, vec!["meta-saved-key-0123456789"]);
        assert_eq!(
            redaction.scrub("saved as meta-saved-key-0123456789 today"),
            "saved as <redacted key> today"
        );
    }

    #[test]
    fn names_decide_which_environment_values_are_secrets() {
        assert!(is_secret_name("META_API_KEY"));
        assert!(is_secret_name("GITHUB_TOKEN"));
        assert!(is_secret_name("DB_PASSWORD_SECRET"));
        assert!(!is_secret_name("ACAN_REPO_NAME"));
        assert!(!is_secret_name("PATH"));
    }

    #[test]
    fn removes_owner_tokens_and_bearer_credentials() {
        let text = "owner acan_0123456789abcdef0123456789abcdef, header Bearer abcdefgh01234567XYZ";
        assert_eq!(
            Redaction::EMPTY.scrub(text),
            "owner <redacted token>, header Bearer <redacted token>"
        );
    }

    #[test]
    fn keeps_short_words_targets_and_urls() {
        let text = "acan_server::observe took task-03 at http://127.0.0.1:7311/api/update";
        assert_eq!(Redaction::EMPTY.scrub(text), text);
    }

    #[test]
    fn replaces_the_repository_and_home_roots() {
        let redaction = Redaction {
            secrets: Vec::new(),
            roots: vec![
                ("/home/example/proj".to_owned(), REPO),
                ("/home/example".to_owned(), HOME),
            ],
        };
        assert_eq!(
            redaction.scrub("reading /home/example/proj/src/main.rs and /home/example/.cache/x"),
            "reading <repo>/src/main.rs and ~/.cache/x"
        );
    }

    #[test]
    fn redacts_user_folder_and_windows_paths() {
        assert_eq!(
            Redaction::EMPTY.scrub("opened /Users/example/notes/.env: denied"),
            "opened <path> denied"
        );
        // A Windows path runs to the end of its line, since its folders can hold spaces.
        assert_eq!(
            Redaction::EMPTY.scrub(r"read C:\src\with space\proj\.env failed"),
            "read <path>"
        );
        assert_eq!(
            Redaction::EMPTY.scrub("GET http://localhost/home/status"),
            "GET http://localhost/home/status"
        );
    }

    #[test]
    fn strips_terminal_colour_codes() {
        assert_eq!(
            Redaction::EMPTY.scrub("\u{1b}[2m2026-11-02\u{1b}[0m INFO started"),
            "2026-11-02 INFO started"
        );
    }
}
