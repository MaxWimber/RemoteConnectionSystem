//! Phase 7: the file API through the real router, with a full session.

use std::process::Command;

use sha2::{Digest, Sha256};

use super::*;
use crate::files::upload::{CHUNK_SIZE, STALE_AFTER};
use crate::util::hex;

const IP: &str = "192.0.2.70";

async fn session() -> (Fixture, String) {
    let f = fixture(Policy::default());
    let cookie = sign_in(&f, IP, 0).await;
    (f, cookie)
}

fn storage(f: &Fixture) -> PathBuf {
    f.dir.join("storage")
}

fn q(path: &str) -> String {
    let mut out = String::new();
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn put_chunk(id: &str, offset: u64, data: &[u8], sha: &str, cookie: &str) -> Request<Body> {
    Request::put(format!("{BASE}/api/uploads/{id}?offset={offset}"))
        .header(header::ORIGIN, ORIGIN)
        .header("x-rcs-request", "1")
        .header("x-forwarded-for", IP)
        .header("x-chunk-sha256", sha)
        .header(header::COOKIE, format!("{COOKIE}={cookie}"))
        .body(Body::from(data.to_vec()))
        .unwrap()
}

fn get_with(path: &str, cookie: &str, extra: &[(&str, &str)]) -> Request<Body> {
    let mut b = Request::get(format!("{BASE}/api{path}"))
        .header("x-forwarded-for", IP)
        .header(header::COOKIE, format!("{COOKIE}={cookie}"));
    for (k, v) in extra {
        b = b.header(*k, *v);
    }
    b.body(Body::empty()).unwrap()
}

/// The raw body, for downloads.
async fn bytes(f: &Fixture, req: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let r = f.router.clone().oneshot(req).await.unwrap();
    let status = r.status();
    let headers = r.headers().clone();
    let body = r.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, headers, body)
}

fn data(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}

fn sha(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

/// Uploads `content` to `path` in protocol-sized chunks and completes it.
async fn upload(f: &Fixture, cookie: &str, path: &str, content: &[u8], overwrite: bool) -> Reply {
    let r = send(
        f,
        post(
            "/uploads",
            json!({ "path": path, "size": content.len(), "overwrite": overwrite }),
            Some(cookie),
            IP,
        ),
    )
    .await;
    if r.status != StatusCode::CREATED {
        return r;
    }
    let id = r.body["id"].as_str().unwrap().to_string();
    for (n, piece) in content.chunks(CHUNK_SIZE as usize).enumerate() {
        let offset = n as u64 * CHUNK_SIZE;
        let r = send(f, put_chunk(&id, offset, piece, &sha(piece), cookie)).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    }
    send(
        f,
        post(
            &format!("/uploads/{id}/complete"),
            json!({}),
            Some(cookie),
            IP,
        ),
    )
    .await
}

#[tokio::test]
async fn folders_rename_and_trash() {
    let (f, c) = session().await;
    let c = c.as_str();
    let r = send(&f, get("/files/list", Some(c))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let names: Vec<&str> = r.body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, [".rcs-trash"], "uploads stay hidden");

    let r = send(
        &f,
        post("/files/mkdir", json!({ "path": "Документы" }), Some(c), IP),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let r = send(
        &f,
        post("/files/mkdir", json!({ "path": "документы" }), Some(c), IP),
    )
    .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_eq!(r.body["error"], "exists");
    for (path, code) in [
        ("../x", "path_dots"),
        ("a/../../x", "path_dots"),
        ("C:/x", "path_stream"),
        ("/x", "path_absolute"),
        ("CON", "path_reserved"),
        (".rcs-uploads/x", "path_internal"),
    ] {
        let r = send(
            &f,
            post("/files/mkdir", json!({ "path": path }), Some(c), IP),
        )
        .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{path}");
        assert_eq!(r.body["error"], code, "{path}");
    }
    let r = send(
        &f,
        post(
            "/files/mkdir",
            json!({ "path": ".rcs-trash/x" }),
            Some(c),
            IP,
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = send(
        &f,
        post("/files/mkdir", json!({ "path": "missing/x" }), Some(c), IP),
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = send(&f, post("/files/mkdir", json!({ "nope": 1 }), Some(c), IP)).await;
    assert_eq!(r.body["error"], "bad_request");

    std::fs::write(storage(&f).join("Документы").join("a.txt"), b"hello").unwrap();
    let rename = |from: &str, to: &str| {
        post(
            "/files/rename",
            json!({ "from": from, "to": to }),
            Some(c),
            IP,
        )
    };
    let r = send(&f, rename("Документы/a.txt", "Документы/b.txt")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let r = send(&f, rename("Документы/b.txt", "Документы/B.TXT")).await;
    assert_eq!(r.status, StatusCode::OK, "case-only rename: {}", r.body);
    assert!(
        std::fs::read_dir(storage(&f).join("Документы"))
            .unwrap()
            .any(|e| e.unwrap().file_name() == "B.TXT")
    );
    std::fs::write(storage(&f).join("other.txt"), b"x").unwrap();
    let r = send(&f, rename("other.txt", "Документы/B.TXT")).await;
    assert_eq!(r.status, StatusCode::CONFLICT, "no overwrite on rename");
    let r = send(&f, rename("Документы", "Документы/sub")).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "into itself");
    let r = send(&f, rename("other.txt", ".rcs-trash/other.txt")).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "into the trash");
    let r = send(&f, rename(".rcs-trash", "trash")).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = send(&f, rename("", "x")).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = send(&f, rename("other.txt", "Документы/moved.txt")).await;
    assert_eq!(r.status, StatusCode::OK, "move to another folder");

    let r = send(
        &f,
        post(
            "/files/delete",
            json!({ "path": "Документы/B.TXT" }),
            Some(c),
            IP,
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let trashed = r.body["trash"].as_str().unwrap().to_string();
    assert!(
        trashed.starts_with(".rcs-trash/") && trashed.ends_with("/B.TXT"),
        "{trashed}"
    );
    assert!(!storage(&f).join("Документы").join("B.TXT").exists());
    let r = send(
        &f,
        get(&format!("/files/list?path={}", q(".rcs-trash")), Some(c)),
    )
    .await;
    assert_eq!(r.body["entries"].as_array().unwrap().len(), 1);
    let r = send(
        &f,
        post("/files/delete", json!({ "path": trashed }), Some(c), IP),
    )
    .await;
    assert_eq!(
        r.status,
        StatusCode::FORBIDDEN,
        "nothing is deleted in the trash"
    );
    let r = send(
        &f,
        post("/files/delete", json!({ "path": "" }), Some(c), IP),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = send(&f, rename(&trashed, "Документы/restored.txt")).await;
    assert_eq!(r.status, StatusCode::OK, "restore from the trash");
    assert_eq!(
        std::fs::read(storage(&f).join("Документы").join("restored.txt")).unwrap(),
        b"hello"
    );
    let r = send(
        &f,
        post("/files/delete", json!({ "path": "Документы" }), Some(c), IP),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "a folder goes to the trash whole");

    let r = send(
        &f,
        post("/files/mkdir", json!({ "path": "x" }), Some(c), IP),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED);
    let mut no_header = post("/files/mkdir", json!({ "path": "y" }), Some(c), IP);
    no_header.headers_mut().remove("x-rcs-request");
    assert_eq!(send(&f, no_header).await.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn upload_resume_download_and_hash() {
    let (f, c) = session().await;
    let c = c.as_str();
    let content = data(2 * CHUNK_SIZE as usize + 12_345);
    let total = content.len() as u64;
    let r = send(
        &f,
        post(
            "/uploads",
            json!({ "path": "big.bin", "size": total }),
            Some(c),
            IP,
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.body["chunk_size"], CHUNK_SIZE);
    let id = r.body["id"].as_str().unwrap().to_string();
    let first = &content[..CHUNK_SIZE as usize];

    let r = send(&f, put_chunk(&id, 0, first, &sha(b"other"), c)).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let r = send(&f, put_chunk(&id, 5, first, &sha(first), c)).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_eq!(r.body["received"], 0);
    let r = send(&f, put_chunk(&id, 0, first, "zz", c)).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = send(&f, put_chunk(&id, 0, first, &sha(first), c)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["received"], CHUNK_SIZE);
    let r = send(&f, put_chunk(&id, 0, first, &sha(first), c)).await;
    assert_eq!(
        r.status,
        StatusCode::CONFLICT,
        "a repeated chunk is refused"
    );
    let too_big = vec![0u8; CHUNK_SIZE as usize + 1];
    let r = send(&f, put_chunk(&id, CHUNK_SIZE, &too_big, &sha(&too_big), c)).await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);

    // A break: the client asks where to go on; this process then "restarts" and loses the running hash.
    let r = send(&f, get(&format!("/uploads/{id}"), Some(c))).await;
    assert_eq!(r.body["received"], CHUNK_SIZE);
    // Resuming from an odd offset works too: the chunk only has to start at `received`.
    let rest = &content[CHUNK_SIZE as usize..CHUNK_SIZE as usize + 1000];
    let r = send(&f, put_chunk(&id, CHUNK_SIZE, rest, &sha(rest), c)).await;
    assert_eq!(r.status, StatusCode::OK);
    f.state.files.uploads.drop_hashes();
    let reloaded = crate::files::upload::Uploads::load(f.state.files.root().ok());
    drop(reloaded);
    let mut at = CHUNK_SIZE + 1000;
    let r = send(
        &f,
        post(&format!("/uploads/{id}/complete"), json!({}), Some(c), IP),
    )
    .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_eq!(r.body["error"], "incomplete");
    while at < total {
        let end = (at + CHUNK_SIZE).min(total);
        let piece = &content[at as usize..end as usize];
        let r = send(&f, put_chunk(&id, at, piece, &sha(piece), c)).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
        at = end;
    }
    let r = send(
        &f,
        post(&format!("/uploads/{id}/complete"), json!({}), Some(c), IP),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["sha256"], sha(&content), "hash read again from disk");
    assert_eq!(r.body["path"], "big.bin");
    assert_eq!(std::fs::read(storage(&f).join("big.bin")).unwrap(), content);
    assert_eq!(
        std::fs::read_dir(storage(&f).join(".rcs-uploads"))
            .unwrap()
            .count(),
        0,
        "nothing left behind"
    );
    let r = send(&f, get(&format!("/uploads/{id}"), Some(c))).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    let r = upload(&f, c, "big.bin", b"new", false).await;
    assert_eq!(
        r.status,
        StatusCode::CONFLICT,
        "no overwrite without asking"
    );
    let small = data(1000);
    let r = upload(&f, c, "small.bin", &small, false).await;
    assert_eq!(r.body["sha256"], sha(&small), "running hash");
    let r = upload(&f, c, "small.bin", b"replaced", true).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(
        std::fs::read(storage(&f).join("small.bin")).unwrap(),
        b"replaced"
    );
    let r = upload(&f, c, "empty.bin", b"", false).await;
    assert_eq!(r.body["sha256"], sha(b""));
    let r = upload(&f, c, ".rcs-trash/x.bin", b"x", false).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = send(
        &f,
        post(
            "/uploads",
            json!({ "path": "huge.bin", "size": 2u64 << 30 }),
            Some(c),
            IP,
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);

    let r = send(
        &f,
        get(&format!("/files/hash?path={}", q("big.bin")), Some(c)),
    )
    .await;
    assert_eq!(r.body["sha256"], sha(&content));
    assert_eq!(r.body["size"], total);

    let url = format!("/files/download?path={}", q("big.bin"));
    let (status, h, body) = bytes(&f, get_with(&url, c, &[])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, content);
    assert_eq!(h[header::ACCEPT_RANGES], "bytes");
    assert_eq!(h[header::CONTENT_TYPE], "application/octet-stream");
    assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(
        h[header::CONTENT_DISPOSITION],
        "attachment; filename=\"big.bin\"; filename*=UTF-8''big.bin"
    );
    let etag = h[header::ETAG].to_str().unwrap().to_string();
    let (status, h, body) = bytes(&f, get_with(&url, c, &[("range", "bytes=10-19")])).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, &content[10..20]);
    assert_eq!(h[header::CONTENT_RANGE], format!("bytes 10-19/{total}"));
    let from = format!("bytes={}-", CHUNK_SIZE + 7);
    let (status, _, body) = bytes(
        &f,
        get_with(&url, c, &[("range", &from), ("if-range", &etag)]),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::PARTIAL_CONTENT,
        "resume with a matching validator"
    );
    assert_eq!(body, &content[CHUNK_SIZE as usize + 7..]);
    let (status, _, body) = bytes(
        &f,
        get_with(&url, c, &[("range", "bytes=0-9"), ("if-range", "\"old\"")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "a changed file comes whole");
    assert_eq!(body.len() as u64, total);
    let (status, h, _) = bytes(&f, get_with(&url, c, &[("range", "bytes=999999999-")])).await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(h[header::CONTENT_RANGE], format!("bytes */{total}"));
    let (status, _, _) = bytes(&f, get_with("/files/download?path=", c, &[])).await;
    assert_eq!(status, StatusCode::CONFLICT, "the root is a folder");
    let (status, _, _) = bytes(
        &f,
        get_with(
            &format!("/files/download?path={}", q("../secrets/auth.json")),
            c,
            &[],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, _) = bytes(
        &f,
        get_with(
            &format!("/files/download?path={}", q("C:/Windows/win.ini")),
            c,
            &[],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, _) = bytes(
        &f,
        get_with(
            &format!("/files/download?path={}", q("..\\secrets\\auth.json")),
            c,
            &[],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn links_lead_nowhere() {
    let (f, c) = session().await;
    let c = c.as_str();
    let made = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(storage(&f).join("out"))
        .arg(f.dir.join("secrets"))
        .output()
        .unwrap();
    assert!(made.status.success());
    let r = send(&f, get("/files/list", Some(c))).await;
    let out = r.body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "out")
        .cloned()
        .unwrap();
    assert_eq!(out["type"], "link");
    let r = send(&f, get(&format!("/files/list?path={}", q("out")), Some(c))).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.body["error"], "link");
    let (status, _, body) = bytes(
        &f,
        get_with(
            &format!("/files/download?path={}", q("out/auth.json")),
            c,
            &[],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!String::from_utf8_lossy(&body).contains("password"));
    let r = send(
        &f,
        get(&format!("/files/hash?path={}", q("out/auth.json")), Some(c)),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = upload(&f, c, "out/new.bin", b"x", false).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = send(
        &f,
        post("/files/mkdir", json!({ "path": "out/x" }), Some(c), IP),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = send(
        &f,
        post(
            "/files/rename",
            json!({ "from": "out/auth.json", "to": "stolen.json" }),
            Some(c),
            IP,
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = send(
        &f,
        post("/files/delete", json!({ "path": "out" }), Some(c), IP),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(f.dir.join("secrets").join("auth.json").exists());
}

#[tokio::test]
async fn stale_uploads_are_swept_and_cancel_cleans_up() {
    let (f, c) = session().await;
    let c = c.as_str();
    let mut ids = Vec::new();
    for name in ["old.bin", "fresh.bin", "cancelled.bin"] {
        let r = send(
            &f,
            post("/uploads", json!({ "path": name, "size": 10 }), Some(c), IP),
        )
        .await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
        ids.push(r.body["id"].as_str().unwrap().to_string());
    }
    let dir = storage(&f).join(".rcs-uploads");
    let old = SystemTime::now() - STALE_AFTER - Duration::from_secs(60);
    std::fs::File::options()
        .write(true)
        .open(dir.join(format!("{}.part", ids[0])))
        .unwrap()
        .set_modified(old)
        .unwrap();
    let root = f.state.files.root().unwrap().clone();
    assert_eq!(f.state.files.uploads.sweep(&root, SystemTime::now()), 1);
    let r = send(&f, get(&format!("/uploads/{}", ids[0]), Some(c))).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert!(!dir.join(format!("{}.json", ids[0])).exists());
    let r = send(&f, get(&format!("/uploads/{}", ids[1]), Some(c))).await;
    assert_eq!(r.status, StatusCode::OK);

    let mut del = post(&format!("/uploads/{}", ids[2]), json!({}), Some(c), IP);
    *del.method_mut() = axum::http::Method::DELETE;
    assert_eq!(send(&f, del).await.status, StatusCode::NO_CONTENT);
    assert!(!dir.join(format!("{}.part", ids[2])).exists());

    // A restart keeps the fresh one and drops stray files.
    std::fs::write(dir.join(format!("{}.part", "0".repeat(32))), b"stray").unwrap();
    let again = crate::files::upload::Uploads::load(Some(&root));
    drop(again);
    assert!(!dir.join(format!("{}.part", "0".repeat(32))).exists());
    assert!(dir.join(format!("{}.part", ids[1])).exists());
    let r = send(&f, get("/uploads/not-an-id", Some(c))).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}
