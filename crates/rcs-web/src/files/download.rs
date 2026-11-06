//! `GET /api/files/download`: one byte range at most, validators for the browser's own resume (D-026).

use std::io::SeekFrom;
use std::os::windows::fs::MetadataExt;
use std::sync::Arc;

use axum::Extension;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use super::ops::PathQuery;
use super::{blocking, parse_path};
use crate::app::AppState;
use crate::auth::{ClientIp, json_error};
use crate::util::http_date;

const STREAM_BUFFER: usize = 256 * 1024;

/// What the `Range` header asks for, after RFC 9110 §14.
#[derive(Debug, PartialEq, Eq)]
pub enum Wanted {
    All,
    /// First and last byte, inclusive.
    Part(u64, u64),
    Unsatisfiable,
}

/// One `bytes=` range. Several ranges, another unit or a malformed header mean the whole file, which the RFC allows.
pub fn parse_range(value: &str, size: u64) -> Wanted {
    let Some(spec) = value.trim().strip_prefix("bytes=") else {
        return Wanted::All;
    };
    if spec.contains(',') {
        return Wanted::All;
    }
    let Some((first, last)) = spec.trim().split_once('-') else {
        return Wanted::All;
    };
    let number = |s: &str| {
        (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse::<u64>().ok())
            .flatten()
    };
    match (first.trim(), last.trim()) {
        ("", suffix) => match number(suffix) {
            Some(0) => Wanted::Unsatisfiable,
            Some(_) if size == 0 => Wanted::Unsatisfiable,
            Some(n) => Wanted::Part(size.saturating_sub(n), size - 1),
            None => Wanted::All,
        },
        (a, "") => match number(a) {
            Some(a) if a < size => Wanted::Part(a, size - 1),
            Some(_) => Wanted::Unsatisfiable,
            None => Wanted::All,
        },
        (a, b) => match (number(a), number(b)) {
            (Some(a), Some(b)) if a > b => Wanted::All,
            (Some(a), Some(_)) if a >= size => Wanted::Unsatisfiable,
            (Some(a), Some(b)) => Wanted::Part(a, b.min(size - 1)),
            _ => Wanted::All,
        },
    }
}

/// `attachment` with an ASCII fallback and the exact name in `filename*` (RFC 6266, RFC 8187).
pub fn content_disposition(name: &str) -> String {
    let fallback: String = name
        .chars()
        .map(|c| {
            if c.is_ascii() && !c.is_ascii_control() && c != '"' && c != '\\' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut encoded = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
            encoded.push(char::from(b));
        } else {
            encoded.push_str(&format!("%{b:02X}"));
        }
    }
    format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

pub async fn download(
    State(state): State<Arc<AppState>>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Query(q): Query<PathQuery>,
    headers: HeaderMap,
) -> Response {
    let root = tri!(state.files.root()).clone();
    let rel = tri!(parse_path(&q.path));
    tracing::debug!(path = %rel, "files download");
    let name = rel.name().unwrap_or("download").to_string();
    let opened = tri!(blocking(move || root.open_file(&rel)).await);
    let size = opened.meta.len();
    let modified = opened.meta.modified().ok();
    let etag = format!("\"{size:x}-{:x}\"", opened.meta.last_write_time());
    let last_modified = modified.map(http_date);

    let mut wanted = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map_or(Wanted::All, |v| parse_range(v, size));
    if let Some(cond) = headers.get(header::IF_RANGE).and_then(|v| v.to_str().ok()) {
        let fresh = cond == etag || last_modified.as_deref() == Some(cond);
        if !fresh {
            wanted = Wanted::All;
        }
    }

    let mut response = Response::builder()
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, &etag)
        .header(header::CACHE_CONTROL, "private, no-cache");
    if let Some(lm) = &last_modified {
        response = response.header(header::LAST_MODIFIED, lm);
    }
    let (status, start, len) = match wanted {
        Wanted::All => (StatusCode::OK, 0, size),
        Wanted::Part(a, b) => {
            response = response.header(header::CONTENT_RANGE, format!("bytes {a}-{b}/{size}"));
            (StatusCode::PARTIAL_CONTENT, a, b - a + 1)
        }
        Wanted::Unsatisfiable => {
            let mut r = json_error(StatusCode::RANGE_NOT_SATISFIABLE, "range_not_satisfiable");
            if let Ok(v) = HeaderValue::from_str(&format!("bytes */{size}")) {
                r.headers_mut().insert(header::CONTENT_RANGE, v);
            }
            return r;
        }
    };
    let mut file = tokio::fs::File::from_std(opened.file);
    if start > 0
        && let Err(e) = file.seek(SeekFrom::Start(start)).await
    {
        tracing::warn!(error = %e, "download seek failed");
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "io_error");
    }
    state.audit.event(
        "file_download",
        Some(ip),
        json!({ "bytes": len, "from": start, "size": size }),
    );
    let body = Body::from_stream(ReaderStream::with_capacity(file.take(len), STREAM_BUFFER));
    response
        .status(status)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_DISPOSITION, content_disposition(&name))
        .header(header::CONTENT_LENGTH, len)
        .body(body)
        .unwrap_or_else(|_| json_error(StatusCode::INTERNAL_SERVER_ERROR, "io_error"))
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        use Wanted::*;
        assert_eq!(parse_range("bytes=0-99", 1000), Part(0, 99));
        assert_eq!(parse_range("bytes=100-", 1000), Part(100, 999));
        assert_eq!(parse_range("bytes=-100", 1000), Part(900, 999));
        assert_eq!(parse_range("bytes=-5000", 1000), Part(0, 999));
        assert_eq!(parse_range("bytes=900-5000", 1000), Part(900, 999));
        assert_eq!(parse_range("bytes=999-999", 1000), Part(999, 999));
        assert_eq!(parse_range("bytes=1000-", 1000), Unsatisfiable);
        assert_eq!(parse_range("bytes=1000-2000", 1000), Unsatisfiable);
        assert_eq!(parse_range("bytes=-0", 1000), Unsatisfiable);
        assert_eq!(parse_range("bytes=0-", 0), Unsatisfiable);
        assert_eq!(parse_range("bytes=-1", 0), Unsatisfiable);
        assert_eq!(parse_range("bytes=0-1,5-6", 1000), All);
        assert_eq!(parse_range("bytes=5-1", 1000), All);
        assert_eq!(parse_range("items=0-1", 1000), All);
        assert_eq!(parse_range("bytes=a-b", 1000), All);
        assert_eq!(parse_range("bytes=+1-2", 1000), All);
        assert_eq!(parse_range("bytes=-", 1000), All);
        assert_eq!(parse_range("bytes=99999999999999999999-", 1000), All);
        let ten_gib = 10 * (1u64 << 30);
        assert_eq!(
            parse_range("bytes=8589934592-", ten_gib),
            Part(8_589_934_592, ten_gib - 1)
        );
    }

    #[test]
    fn disposition() {
        assert_eq!(
            content_disposition("report.pdf"),
            "attachment; filename=\"report.pdf\"; filename*=UTF-8''report.pdf"
        );
        assert_eq!(
            content_disposition("отчёт \"1\".txt"),
            "attachment; filename=\"_____ _1_.txt\"; \
             filename*=UTF-8''%D0%BE%D1%82%D1%87%D1%91%D1%82%20%221%22.txt"
        );
        assert!(!content_disposition("a\r\nSet-Cookie: x").contains('\r'));
    }
}
