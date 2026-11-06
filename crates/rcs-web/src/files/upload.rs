//! Uploads in 8 MiB chunks with resume (docs/PROTOCOL.md «Загрузка с докачкой», D-026).
//!
//! An upload is `.rcs-uploads/<id>.json` (what it becomes) and `.rcs-uploads/<id>.part` (the bytes so far). Only a
//! chunk whose SHA-256 matched is written, so the length of `.part` is how much was received, also after a crash.
//! `complete` checks the size, hashes the whole file and renames it into place within the volume.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use axum::Extension;
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::core::PCWSTR;

use super::ops::{hash_file, move_path};
use super::path::{FsError, RelPath, Root};
use super::{blocking, fs_error, parse_json, parse_path};
use crate::app::AppState;
use crate::auth::{ClientIp, json_error};
use crate::util::{hex, random, rfc3339, unhex};

pub const CHUNK_SIZE: u64 = 8 * 1024 * 1024;
/// Unfinished uploads untouched this long are removed (P7-5).
pub const STALE_AFTER: Duration = Duration::from_secs(7 * 24 * 3600);
const SWEEP_EVERY: Duration = Duration::from_secs(3600);
const MAX_ACTIVE: usize = 32;

#[derive(Serialize, Deserialize, Clone)]
struct Meta {
    path: String,
    size: u64,
    overwrite: bool,
    created_at: String,
}

struct Upload {
    meta: Meta,
    received: u64,
    /// SHA-256 of bytes `0..received`, while every chunk went through this process; `None` after a restart or a
    /// failed write, and then `complete` reads the file again.
    hasher: Option<Sha256>,
}

#[derive(Default)]
pub struct Uploads {
    map: Mutex<HashMap<String, Arc<tokio::sync::Mutex<Upload>>>>,
}

fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn files_of(dir: &Path, id: &str) -> (PathBuf, PathBuf) {
    (
        dir.join(format!("{id}.json")),
        dir.join(format!("{id}.part")),
    )
}

fn is_stale(part: &Path, now: SystemTime) -> bool {
    std::fs::metadata(part)
        .and_then(|m| m.modified())
        .is_ok_and(|t| now.duration_since(t).unwrap_or_default() > STALE_AFTER)
}

fn remove(dir: &Path, id: &str) {
    let (json, part) = files_of(dir, id);
    let _ = std::fs::remove_file(part);
    let _ = std::fs::remove_file(json);
}

impl Uploads {
    /// Picks up the uploads a previous run left; stale ones and stray files go.
    pub fn load(root: Option<&Root>) -> Self {
        let uploads = Self::default();
        let Some(root) = root else {
            return uploads;
        };
        let dir = root.uploads();
        let Ok(items) = std::fs::read_dir(&dir) else {
            return uploads;
        };
        let now = SystemTime::now();
        let mut map = HashMap::new();
        let mut stray = Vec::new();
        for item in items.flatten() {
            let name = item.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".json") else {
                if let Some(id) = name.strip_suffix(".part")
                    && valid_id(id)
                    && !dir.join(format!("{id}.json")).exists()
                {
                    stray.push(item.path());
                }
                continue;
            };
            if !valid_id(id) {
                continue;
            }
            let (json, part) = files_of(&dir, id);
            let meta = std::fs::read(&json)
                .ok()
                .and_then(|b| serde_json::from_slice::<Meta>(&b).ok());
            let received = std::fs::metadata(&part).map(|m| m.len()).ok();
            match (meta, received) {
                (Some(meta), Some(received)) if received <= meta.size && !is_stale(&part, now) => {
                    let upload = Upload {
                        meta,
                        received,
                        hasher: None,
                    };
                    map.insert(id.to_string(), Arc::new(tokio::sync::Mutex::new(upload)));
                }
                _ => remove(&dir, id),
            }
        }
        for p in stray {
            let _ = std::fs::remove_file(p);
        }
        tracing::info!(uploads = map.len(), "unfinished uploads loaded");
        *uploads.map.lock().expect("uploads lock") = map;
        uploads
    }

    fn get(&self, id: &str) -> Option<Arc<tokio::sync::Mutex<Upload>>> {
        self.map.lock().ok()?.get(id).cloned()
    }

    fn forget(&self, id: &str) {
        if let Ok(mut m) = self.map.lock() {
            m.remove(id);
        }
    }

    fn len(&self) -> usize {
        self.map.lock().map_or(0, |m| m.len())
    }

    /// As after a restart: `complete` has to read the file again.
    #[cfg(test)]
    pub fn drop_hashes(&self) {
        for u in self.map.lock().expect("uploads lock").values() {
            u.try_lock().expect("idle upload").hasher = None;
        }
    }

    /// Removes uploads whose `.part` has not changed for `STALE_AFTER`; one in use right now is left alone.
    pub fn sweep(&self, root: &Root, now: SystemTime) -> usize {
        let dir = root.uploads();
        let entries: Vec<_> = match self.map.lock() {
            Ok(m) => m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            Err(_) => return 0,
        };
        let mut removed = 0;
        for (id, upload) in entries {
            let Ok(_busy) = upload.try_lock() else {
                continue;
            };
            if is_stale(&files_of(&dir, &id).1, now) {
                self.forget(&id);
                remove(&dir, &id);
                removed += 1;
            }
        }
        removed
    }
}

/// Hourly sweep (P7-5). The start-up one happens in `Uploads::load`.
pub fn spawn_sweeper(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SWEEP_EVERY);
        tick.tick().await;
        loop {
            tick.tick().await;
            let Ok(root) = state.files.root().cloned() else {
                return;
            };
            let s = state.clone();
            let removed = tokio::task::spawn_blocking(move || {
                s.files.uploads.sweep(&root, SystemTime::now())
            })
            .await
            .unwrap_or(0);
            if removed > 0 {
                tracing::info!(removed, "stale uploads removed");
                state
                    .audit
                    .event("file_upload_expired", None, json!({ "count": removed }));
            }
        }
    });
}

fn free_bytes(dir: &Path) -> Option<u64> {
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
    let mut free = 0u64;
    // SAFETY: a NUL-terminated string and a writable u64, both alive for the call.
    unsafe { GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&raw mut free), None, None) }.ok()?;
    Some(free)
}

/// Where the upload ends up, checked now and again at `complete`.
fn target(root: &Root, rel: &RelPath, overwrite: bool) -> Result<PathBuf, FsError> {
    if rel.is_root() || rel.in_trash() {
        return Err(FsError::Forbidden);
    }
    match root.vacant(rel) {
        Err(FsError::Exists) if overwrite => Ok(root.open_file(rel)?.path),
        other => other,
    }
}

#[derive(Deserialize)]
struct CreateBody {
    path: String,
    size: u64,
    #[serde(default)]
    overwrite: bool,
}

pub async fn create(
    State(state): State<Arc<AppState>>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    body: Bytes,
) -> Response {
    let root = tri!(state.files.root()).clone();
    let b: CreateBody = tri!(parse_json(&body));
    let rel = tri!(parse_path(&b.path));
    tracing::debug!(path = %rel, size = b.size, "upload create");
    if b.size > state.files.max_upload {
        return json_error(StatusCode::PAYLOAD_TOO_LARGE, "too_large");
    }
    if state.files.uploads.len() >= MAX_ACTIVE {
        return json_error(StatusCode::TOO_MANY_REQUESTS, "too_many_uploads");
    }
    let meta = Meta {
        path: rel.to_string(),
        size: b.size,
        overwrite: b.overwrite,
        created_at: rfc3339(SystemTime::now()),
    };
    let id = hex(&random::<16>());
    let saved = meta.clone();
    let id2 = id.clone();
    let made = blocking(move || {
        target(&root, &rel, saved.overwrite)?;
        if free_bytes(root.path()).is_some_and(|f| f < saved.size) {
            return Ok(false);
        }
        let (json, part) = files_of(&root.uploads(), &id2);
        let text = serde_json::to_vec(&saved).map_err(|e| FsError::Io(e.into()))?;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&json)?
            .write_all(&text)?;
        if let Err(e) = OpenOptions::new().write(true).create_new(true).open(&part) {
            let _ = std::fs::remove_file(&json);
            return Err(e.into());
        }
        Ok(true)
    })
    .await;
    if !tri!(made) {
        return json_error(StatusCode::INSUFFICIENT_STORAGE, "disk_full");
    }
    let size = meta.size;
    let upload = Upload {
        meta,
        received: 0,
        hasher: Some(Sha256::new()),
    };
    if let Ok(mut m) = state.files.uploads.map.lock() {
        m.insert(id.clone(), Arc::new(tokio::sync::Mutex::new(upload)));
    }
    state
        .audit
        .event("file_upload_started", Some(ip), json!({ "size": size }));
    (
        StatusCode::CREATED,
        axum::Json(json!({ "id": id, "chunk_size": CHUNK_SIZE, "received": 0 })),
    )
        .into_response()
}

fn not_found() -> Response {
    json_error(StatusCode::NOT_FOUND, "not_found")
}

fn lookup(state: &AppState, id: &str) -> Result<Arc<tokio::sync::Mutex<Upload>>, Response> {
    if !valid_id(id) {
        return Err(not_found());
    }
    state.files.uploads.get(id).ok_or_else(not_found)
}

pub async fn status(State(state): State<Arc<AppState>>, UrlPath(id): UrlPath<String>) -> Response {
    let upload = tri!(lookup(&state, &id));
    let u = upload.lock().await;
    axum::Json(json!({ "received": u.received, "size": u.meta.size })).into_response()
}

#[derive(Deserialize)]
pub struct OffsetQuery {
    offset: u64,
}

pub async fn chunk(
    State(state): State<Arc<AppState>>,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<OffsetQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let root = tri!(state.files.root()).clone();
    let upload = tri!(lookup(&state, &id));
    let Some(expected) = headers
        .get("x-chunk-sha256")
        .and_then(|v| v.to_str().ok())
        .and_then(unhex)
        .filter(|h| h.len() == 32)
    else {
        return json_error(StatusCode::BAD_REQUEST, "chunk_hash_missing");
    };
    let mut u = upload.lock().await;
    if q.offset != u.received {
        return (
            StatusCode::CONFLICT,
            axum::Json(json!({ "error": "offset_mismatch", "received": u.received })),
        )
            .into_response();
    }
    let len = body.len() as u64;
    if len == 0 || len > CHUNK_SIZE || q.offset + len > u.meta.size {
        return json_error(StatusCode::BAD_REQUEST, "bad_chunk");
    }
    let mut hasher = u.hasher.take();
    let offset = q.offset;
    let part = files_of(&root.uploads(), &id).1;
    let written = tokio::task::spawn_blocking(move || {
        if Sha256::digest(&body).as_slice() != expected.as_slice() {
            return (Ok(false), hasher);
        }
        let write = || -> std::io::Result<()> {
            let mut f = OpenOptions::new().write(true).open(&part)?;
            // A length beyond `received` can only be the tail of a write that failed; it is cut off.
            if f.metadata()?.len() != offset {
                f.set_len(offset)?;
            }
            f.seek(SeekFrom::Start(offset))?;
            f.write_all(&body)
        };
        match write() {
            Ok(()) => {
                if let Some(h) = hasher.as_mut() {
                    h.update(&body);
                }
                (Ok(true), hasher)
            }
            Err(e) => (Err(e), None),
        }
    })
    .await;
    let (result, hasher) = match written {
        Ok(r) => r,
        Err(_) => (Err(std::io::Error::other("task failed")), None),
    };
    u.hasher = hasher;
    match result {
        Ok(true) => {
            u.received += len;
            axum::Json(json!({ "received": u.received })).into_response()
        }
        Ok(false) => json_error(StatusCode::UNPROCESSABLE_ENTITY, "chunk_hash_mismatch"),
        Err(e) => fs_error(e.into()),
    }
}

pub async fn complete(
    State(state): State<Arc<AppState>>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let root = tri!(state.files.root()).clone();
    let upload = tri!(lookup(&state, &id));
    let mut u = upload.lock().await;
    if u.received != u.meta.size {
        return (
            StatusCode::CONFLICT,
            axum::Json(json!({ "error": "incomplete", "received": u.received })),
        )
            .into_response();
    }
    let hasher = u.hasher.take();
    // Reading 10 GB again takes a while; one such read at a time.
    let _one = if hasher.is_none() {
        Some(state.files.hash_lock.lock().await)
    } else {
        None
    };
    let meta = u.meta.clone();
    let id2 = id.clone();
    let done = blocking(move || {
        let dir = root.uploads();
        let (json, part) = files_of(&dir, &id2);
        let mut file = OpenOptions::new().read(true).write(true).open(&part)?;
        if file.metadata()?.len() != meta.size {
            return Err(FsError::Io(std::io::Error::other("part size changed")));
        }
        let sha256 = match hasher {
            Some(h) => hex(&h.finalize()),
            None => hash_file(&mut file)?.0,
        };
        file.sync_all()?;
        drop(file);
        let rel = RelPath::parse(&meta.path).map_err(|_| FsError::Forbidden)?;
        let to = target(&root, &rel, meta.overwrite)?;
        move_path(&part, &to, meta.overwrite)?;
        let _ = std::fs::remove_file(json);
        Ok(sha256)
    })
    .await;
    let sha256 = match done {
        Ok(s) => s,
        Err(r) => return r,
    };
    state.files.uploads.forget(&id);
    state.audit.event(
        "file_upload_completed",
        Some(ip),
        json!({ "size": u.meta.size }),
    );
    axum::Json(json!({ "path": u.meta.path, "sha256": sha256 })).into_response()
}

pub async fn cancel(
    State(state): State<Arc<AppState>>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let root = tri!(state.files.root()).clone();
    let upload = tri!(lookup(&state, &id));
    let _u = upload.lock().await;
    state.files.uploads.forget(&id);
    let dir = root.uploads();
    let _ = tokio::task::spawn_blocking(move || remove(&dir, &id)).await;
    state
        .audit
        .event("file_upload_cancelled", Some(ip), json!({}));
    StatusCode::NO_CONTENT.into_response()
}
