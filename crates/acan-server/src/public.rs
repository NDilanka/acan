//! The public files: one dedicated directory served to anyone, without a token.
//!
//! Nothing outside that directory may be reached. The root may not contain the repository or
//! its `.acan` state directory, and every requested file is resolved through symlinks and
//! junctions before it is served, so a link inside the root cannot point out of it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, ensure};
use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use percent_encoding::percent_decode_str;
use tower_http::services::ServeFile;

/// The directory index served for a request that names a directory.
const INDEX: &str = "index.html";

/// A canonical public directory, checked to hold no private state.
#[derive(Debug)]
pub(crate) struct PublicRoot {
    root: PathBuf,
}

impl PublicRoot {
    /// Accepts `web` as the public root for `repo`.
    ///
    /// # Errors
    ///
    /// Returns an error when `web` does not exist, contains the repository or the state
    /// directory, or lies inside the state directory.
    pub(crate) fn new(web: &Path, repo: &Path) -> anyhow::Result<Self> {
        let checked = Self::open(web)?;
        let repo = repo
            .canonicalize()
            .with_context(|| format!("opening the repository {}", repo.display()))?;
        let state = repo.join(crate::STATE_DIR);
        ensure!(
            !repo.starts_with(&checked.root),
            "the web directory {} contains the repository; give it a dedicated public directory",
            web.display()
        );
        ensure!(
            !checked.root.starts_with(&state),
            "the web directory {} is inside the private state directory",
            web.display()
        );
        Ok(checked)
    }

    /// Accepts `web` as the public root before any repository is known. A server with no project
    /// still serves its page from it. Each private folder must then be checked with
    /// [`keep_out`](Self::keep_out).
    ///
    /// # Errors
    ///
    /// Returns an error when `web` does not exist.
    pub(crate) fn open(web: &Path) -> anyhow::Result<Self> {
        let root = web
            .canonicalize()
            .with_context(|| format!("opening the web directory {}", web.display()))?;
        Ok(Self { root })
    }

    /// Checks that the private folder `dir`, such as the user's folder with its tokens and keys,
    /// neither sits inside the root nor holds it.
    ///
    /// # Errors
    ///
    /// Returns an error when `dir` does not exist, or when it and the root overlap.
    pub(crate) fn keep_out(&self, dir: &Path) -> anyhow::Result<()> {
        let private = dir
            .canonicalize()
            .with_context(|| format!("opening {}", dir.display()))?;
        ensure!(
            !private.starts_with(&self.root),
            "the web directory {} contains the private folder {}; give it a dedicated public directory",
            self.root.display(),
            dir.display()
        );
        ensure!(
            !self.root.starts_with(&private),
            "the web directory {} is inside the private folder {}",
            self.root.display(),
            dir.display()
        );
        Ok(())
    }

    /// The file a request path names, or `None` when it is missing, not a plain path, or
    /// resolves outside the root.
    async fn resolve(&self, path: &str) -> Option<PathBuf> {
        let decoded = percent_decode_str(path).decode_utf8().ok()?;
        let mut candidate = self.root.clone();
        for part in decoded.split('/') {
            match part {
                "" | "." => {}
                ".." => return None,
                // A backslash or colon would let one segment act as a path or drive on Windows.
                part if part.contains(['\\', ':']) => return None,
                part => candidate.push(part),
            }
        }
        let mut file = tokio::fs::canonicalize(&candidate).await.ok()?;
        if tokio::fs::metadata(&file).await.ok()?.is_dir() {
            file = tokio::fs::canonicalize(file.join(INDEX)).await.ok()?;
        }
        let is_file = tokio::fs::metadata(&file).await.ok()?.is_file();
        (is_file && file.starts_with(&self.root)).then_some(file)
    }
}

/// Serves the public file a request names, or 404.
pub(crate) async fn serve(root: Arc<PublicRoot>, request: Request) -> Response {
    let Some(file) = root.resolve(request.uri().path()).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match ServeFile::new(file).try_call(request).await {
        Ok(response) => response.map(Body::new),
        Err(err) => {
            tracing::warn!(error = %err, "serving a public file failed");
            StatusCode::NOT_FOUND.into_response()
        }
    }
}
