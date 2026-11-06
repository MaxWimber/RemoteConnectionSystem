//! The file API under `files.root` (docs/PROTOCOL.md «Файлы», docs/SECURITY.md «Файлы», D-026).
//!
//! Every disk operation runs in `spawn_blocking`; request handlers only parse, check and answer. Paths are written to
//! the `rcs-web` log at `debug` only and never to the audit log.

// `Result<_, Response>` carries the ready answer of a handler; it is moved once per request, so boxing it buys nothing.
#![allow(clippy::result_large_err)]

/// Returns the response held in `Err` from the enclosing handler.
macro_rules! tri {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(r) => return r,
        }
    };
}
pub mod download;
pub mod ops;
pub mod path;
pub mod upload;

use std::path::Path;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use serde::de::DeserializeOwned;
use std::sync::Arc;

use crate::app::AppState;
use crate::auth::json_error;
use path::{FsError, PathError, RelPath, Root};

pub struct Files {
    root: Result<Root, String>,
    pub max_upload: u64,
    /// Whole-file hashes read the disk one at a time, so two of them do not compete with each other and with uploads.
    pub hash_lock: tokio::sync::Mutex<()>,
    pub uploads: upload::Uploads,
}

impl Files {
    /// A root that fails the start-up check leaves the file API answering `503`; sign-in and video still work.
    pub fn new(dir: &Path, system_drive: Option<char>, max_upload: u64) -> Self {
        let root = Root::new(dir, system_drive);
        match &root {
            Ok(r) => tracing::info!(root = %r.path().display(), "files root checked"),
            Err(e) => tracing::error!(error = %e, "files root refused, the file API is off"),
        }
        let uploads = upload::Uploads::load(root.as_ref().ok());
        Self {
            root,
            max_upload,
            hash_lock: tokio::sync::Mutex::new(()),
            uploads,
        }
    }

    pub fn root(&self) -> Result<&Root, Response> {
        self.root
            .as_ref()
            .map_err(|_| json_error(StatusCode::SERVICE_UNAVAILABLE, "files_unavailable"))
    }
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/files/list", get(ops::list))
        .route("/files/download", get(download::download))
        .route("/files/hash", get(ops::hash))
        .route("/files/mkdir", post(ops::mkdir))
        .route("/files/rename", post(ops::rename_handler))
        .route("/files/delete", post(ops::delete_handler))
        .route("/uploads", post(upload::create))
        .route(
            "/uploads/{id}",
            get(upload::status)
                .put(upload::chunk)
                .delete(upload::cancel)
                .layer(DefaultBodyLimit::max(upload::CHUNK_SIZE as usize)),
        )
        .route("/uploads/{id}/complete", post(upload::complete))
}

pub fn path_error(e: PathError) -> Response {
    json_error(StatusCode::BAD_REQUEST, e.code())
}

pub fn fs_error(e: FsError) -> Response {
    match e {
        FsError::NotFound => json_error(StatusCode::NOT_FOUND, "not_found"),
        FsError::Exists => json_error(StatusCode::CONFLICT, "exists"),
        FsError::NotDir => json_error(StatusCode::CONFLICT, "not_a_directory"),
        FsError::IsDir => json_error(StatusCode::CONFLICT, "is_a_directory"),
        FsError::Link => json_error(StatusCode::FORBIDDEN, "link"),
        FsError::Mismatch => json_error(StatusCode::FORBIDDEN, "path_mismatch"),
        FsError::Forbidden => json_error(StatusCode::FORBIDDEN, "forbidden"),
        FsError::Io(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            json_error(StatusCode::FORBIDDEN, "access_denied")
        }
        FsError::Io(e) if e.raw_os_error() == Some(112) => {
            // ERROR_DISK_FULL
            json_error(StatusCode::INSUFFICIENT_STORAGE, "disk_full")
        }
        FsError::Io(e) => {
            tracing::warn!(error = %e, "file operation failed");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "io_error")
        }
    }
}

pub fn parse_path(text: &str) -> Result<RelPath, Response> {
    RelPath::parse(text).map_err(path_error)
}

/// A JSON body; a malformed one is `400 bad_request` in the API's own error form.
pub fn parse_json<T: DeserializeOwned>(body: &[u8]) -> Result<T, Response> {
    serde_json::from_slice(body).map_err(|_| json_error(StatusCode::BAD_REQUEST, "bad_request"))
}

/// Runs disk work off the async threads.
pub async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, FsError> + Send + 'static,
) -> Result<T, Response> {
    match tokio::task::spawn_blocking(f).await {
        Ok(r) => r.map_err(fs_error),
        Err(e) => {
            tracing::error!(error = %e, "file task failed");
            Err(json_error(StatusCode::INTERNAL_SERVER_ERROR, "io_error"))
        }
    }
}
