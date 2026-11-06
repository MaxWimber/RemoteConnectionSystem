//! Folders and objects: list, create, rename and move, delete into `.rcs-trash`, whole-file SHA-256.

use std::io::Read;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use axum::Extension;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use windows::Win32::Storage::FileSystem::{
    MOVE_FILE_FLAGS, MOVEFILE_REPLACE_EXISTING, MoveFileExW,
};
use windows::core::PCWSTR;

use super::path::{self, FsError, RelPath, Root, TRASH, UPLOADS};
use super::{blocking, parse_json, parse_path};
use crate::app::AppState;
use crate::auth::ClientIp;
use crate::util::{hex, random, rfc3339};

const READ_BUFFER: usize = 1 << 20;

#[derive(Deserialize)]
pub struct PathQuery {
    #[serde(default)]
    pub path: String,
}

#[derive(Deserialize)]
struct PathBody {
    path: String,
}

#[derive(Deserialize)]
struct RenameBody {
    from: String,
    to: String,
}

/// One `list` entry, see docs/PROTOCOL.md.
fn entry(name: &str, meta: &std::fs::Metadata) -> Value {
    let kind = if path::is_link(meta) {
        "link"
    } else if meta.is_dir() {
        "dir"
    } else {
        "file"
    };
    let size = if kind == "file" { meta.len() } else { 0 };
    let modified = meta.modified().map(rfc3339).unwrap_or_default();
    json!({ "name": name, "type": kind, "size": size, "modified_at": modified })
}

pub fn list_dir(root: &Root, rel: &RelPath) -> Result<Vec<Value>, FsError> {
    let dir = root.open_dir(rel)?;
    let mut out: Vec<(bool, String, Value)> = Vec::new();
    for item in std::fs::read_dir(&dir.path)? {
        let item = item?;
        // A name the API cannot address (not UTF-16 that maps to UTF-8, a trailing dot made through `\\?\`) is left
        // out rather than shown and then refused.
        let Some(name) = item.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if path::check_name(&name).is_err() || (rel.is_root() && name.eq_ignore_ascii_case(UPLOADS))
        {
            continue;
        }
        // On Windows this is the directory entry itself: a link is not followed.
        let meta = item.metadata()?;
        let is_dir = meta.is_dir() && !path::is_link(&meta);
        out.push((is_dir, name.to_lowercase(), entry(&name, &meta)));
    }
    out.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    Ok(out.into_iter().map(|(_, _, v)| v).collect())
}

/// `MoveFileExW` within the volume: atomic, and without `replace` it fails when the target exists.
pub fn move_path(from: &Path, to: &Path, replace: bool) -> std::io::Result<()> {
    let wide = |p: &Path| p.as_os_str().encode_wide().chain([0]).collect::<Vec<u16>>();
    let (from, to) = (wide(from), wide(to));
    let flags = if replace {
        MOVEFILE_REPLACE_EXISTING
    } else {
        MOVE_FILE_FLAGS(0)
    };
    // SAFETY: two NUL-terminated strings that live for the call.
    unsafe { MoveFileExW(PCWSTR(from.as_ptr()), PCWSTR(to.as_ptr()), flags) }
        .map_err(|e| std::io::Error::from_raw_os_error(e.code().0 & 0xffff))
}

pub fn make_dir(root: &Root, rel: &RelPath) -> Result<(), FsError> {
    if rel.in_trash() {
        return Err(FsError::Forbidden);
    }
    let path = root.vacant(rel)?;
    std::fs::create_dir(path)?;
    Ok(())
}

pub fn rename(root: &Root, from: &RelPath, to: &RelPath) -> Result<(), FsError> {
    let trash_itself = from.names().len() == 1 && from.in_trash();
    if from.is_root() || to.is_root() || trash_itself || to.in_trash() {
        return Err(FsError::Forbidden);
    }
    let source = root.open(from)?;
    let source_path = source.path.clone();
    drop(source);
    let case_only = from.names().len() == to.names().len() && to.within(from);
    if !case_only && to.within(from) {
        return Err(FsError::Forbidden);
    }
    let target = if case_only {
        // Same object, other letter case: the target "exists" because it is the source.
        root.full(to)
    } else {
        root.vacant(to)?
    };
    move_path(&source_path, &target, false)?;
    Ok(())
}

/// `.rcs-trash/20261107-120000-ab12/<name>`; returns that path.
pub fn delete(root: &Root, rel: &RelPath) -> Result<RelPath, FsError> {
    if rel.is_root() || rel.in_trash() {
        return Err(FsError::Forbidden);
    }
    let name = rel.name().ok_or(FsError::Forbidden)?.to_string();
    let source = root.open(rel)?;
    let source_path = source.path.clone();
    drop(source);
    let t = rfc3339(SystemTime::now());
    let bucket = format!(
        "{}{}{}-{}{}{}-{}",
        &t[0..4],
        &t[5..7],
        &t[8..10],
        &t[11..13],
        &t[14..16],
        &t[17..19],
        hex(&random::<2>())
    );
    let folder = RelPath::root()
        .join(TRASH)
        .and_then(|p| p.join(&bucket))
        .map_err(|_| FsError::Forbidden)?;
    let target = folder.join(&name).map_err(|_| FsError::Forbidden)?;
    std::fs::create_dir(root.vacant(&folder)?)?;
    if let Err(e) = move_path(&source_path, &root.full(&target), false) {
        let _ = std::fs::remove_dir(root.full(&folder));
        return Err(e.into());
    }
    Ok(target)
}

/// SHA-256 and length of an open file, read from the start.
pub fn hash_file(file: &mut std::fs::File) -> std::io::Result<(String, u64)> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; READ_BUFFER];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((hex(&hasher.finalize()), total))
}

pub async fn list(State(state): State<Arc<AppState>>, Query(q): Query<PathQuery>) -> Response {
    let root = tri!(state.files.root()).clone();
    let rel = tri!(parse_path(&q.path));
    tracing::debug!(path = %rel, "files list");
    let shown = rel.to_string();
    let entries = tri!(blocking(move || list_dir(&root, &rel)).await);
    axum::Json(json!({ "path": shown, "entries": entries })).into_response()
}

pub async fn hash(State(state): State<Arc<AppState>>, Query(q): Query<PathQuery>) -> Response {
    let root = tri!(state.files.root()).clone();
    let rel = tri!(parse_path(&q.path));
    tracing::debug!(path = %rel, "files hash");
    let _one = state.files.hash_lock.lock().await;
    let (sha256, size) = tri!(
        blocking(move || {
            let mut o = root.open_file(&rel)?;
            Ok(hash_file(&mut o.file)?)
        })
        .await
    );
    axum::Json(json!({ "sha256": sha256, "size": size })).into_response()
}

pub async fn mkdir(
    State(state): State<Arc<AppState>>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    body: Bytes,
) -> Response {
    let root = tri!(state.files.root()).clone();
    let b: PathBody = tri!(parse_json(&body));
    let rel = tri!(parse_path(&b.path));
    tracing::debug!(path = %rel, "files mkdir");
    let shown = rel.to_string();
    tri!(blocking(move || make_dir(&root, &rel)).await);
    state.audit.event("file_mkdir", Some(ip), json!({}));
    (StatusCode::CREATED, axum::Json(json!({ "path": shown }))).into_response()
}

pub async fn rename_handler(
    State(state): State<Arc<AppState>>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    body: Bytes,
) -> Response {
    let root = tri!(state.files.root()).clone();
    let b: RenameBody = tri!(parse_json(&body));
    let from = tri!(parse_path(&b.from));
    let to = tri!(parse_path(&b.to));
    tracing::debug!(from = %from, to = %to, "files rename");
    let shown = to.to_string();
    tri!(blocking(move || rename(&root, &from, &to)).await);
    state.audit.event("file_rename", Some(ip), json!({}));
    axum::Json(json!({ "path": shown })).into_response()
}

pub async fn delete_handler(
    State(state): State<Arc<AppState>>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    body: Bytes,
) -> Response {
    let root = tri!(state.files.root()).clone();
    let b: PathBody = tri!(parse_json(&body));
    let rel = tri!(parse_path(&b.path));
    tracing::debug!(path = %rel, "files delete");
    let moved = tri!(blocking(move || delete(&root, &rel)).await);
    state.audit.event("file_delete", Some(ip), json!({}));
    axum::Json(json!({ "trash": moved.to_string() })).into_response()
}
