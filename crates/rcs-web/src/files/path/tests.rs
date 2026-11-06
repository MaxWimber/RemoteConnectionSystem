use std::path::Path;
use std::process::Command;

use super::*;

const BAD: &[(&str, PathError)] = &[
    ("..", PathError::Dots),
    ("../x", PathError::Dots),
    ("a/../../x", PathError::Dots),
    ("a/..", PathError::Dots),
    (".", PathError::Dots),
    ("a/./b", PathError::Dots),
    ("/", PathError::Absolute),
    ("/etc/passwd", PathError::Absolute),
    ("//server/share", PathError::Absolute),
    ("C:/Windows", PathError::Stream),
    ("C:", PathError::Stream),
    ("C:x", PathError::Stream),
    ("file.txt:Zone.Identifier", PathError::Stream),
    ("file.txt::$DATA", PathError::Stream),
    ("a\\..\\..\\b", PathError::Separator),
    ("\\\\?\\C:\\Windows", PathError::Separator),
    ("..\\x", PathError::Separator),
    ("a//b", PathError::Empty),
    ("a/", PathError::Empty),
    ("CON", PathError::Reserved),
    ("con", PathError::Reserved),
    ("con.txt", PathError::Reserved),
    ("Docs/nul", PathError::Reserved),
    ("NUL.tar.gz", PathError::Reserved),
    ("CON .txt", PathError::Reserved),
    ("COM1", PathError::Reserved),
    ("com0", PathError::Reserved),
    ("lpt9.log", PathError::Reserved),
    ("COM¹", PathError::Reserved),
    ("lpt³.txt", PathError::Reserved),
    ("PRN", PathError::Reserved),
    ("aux.c", PathError::Reserved),
    ("CONIN$", PathError::Reserved),
    ("conout$.x", PathError::Reserved),
    ("a.", PathError::TrailingDotOrSpace),
    ("a ", PathError::TrailingDotOrSpace),
    ("a/b./c", PathError::TrailingDotOrSpace),
    ("...", PathError::TrailingDotOrSpace),
    ("a\0b", PathError::Character),
    ("a\nb", PathError::Character),
    ("a\u{7f}", PathError::Character),
    ("a\u{85}", PathError::Character),
    ("a<b", PathError::Character),
    ("a>b", PathError::Character),
    ("a\"b", PathError::Character),
    ("a|b", PathError::Character),
    ("a?b", PathError::Character),
    ("*.*", PathError::Character),
    (".rcs-uploads", PathError::Internal),
    (".RCS-Uploads/x.part", PathError::Internal),
];

const GOOD: &[&str] = &[
    "",
    "a",
    "Документы/отчёт за 2026.pdf",
    ".rcs-trash",
    ".rcs-trash/20261107-120000-ab12/x",
    "a/.rcs-uploads",
    "COM10",
    "CONSOLE",
    "com",
    "nullable.txt",
    "x.con",
    "file..txt",
    " leading space",
    ".hidden",
    "😀 emoji",
    "a'b;c&d%e#f",
];

#[test]
fn malicious_paths_are_refused() {
    for (text, why) in BAD {
        assert_eq!(RelPath::parse(text), Err(*why), "{text:?}");
    }
}

#[test]
fn ordinary_paths_pass() {
    for text in GOOD {
        let p = RelPath::parse(text).unwrap_or_else(|e| panic!("{text:?}: {e:?}"));
        assert_eq!(p.to_string(), *text);
    }
}

#[test]
fn length_limits() {
    let name = "я".repeat(MAX_NAME_UNITS);
    assert!(RelPath::parse(&name).is_ok());
    assert_eq!(RelPath::parse(&format!("{name}я")), Err(PathError::TooLong));
    // Four bytes in UTF-8, two units in UTF-16.
    assert_eq!(RelPath::parse(&"😀".repeat(128)), Err(PathError::TooLong));
    let long = vec!["a".repeat(200); 21].join("/");
    assert!(long.len() > MAX_PATH_BYTES);
    assert_eq!(RelPath::parse(&long), Err(PathError::TooLong));
    let deep = RelPath::parse(&vec!["b".repeat(200); 20].join("/")).unwrap();
    assert_eq!(deep.join(&"c".repeat(200)), Err(PathError::TooLong));
}

#[test]
fn join_parent_within() {
    let root = RelPath::root();
    let docs = root.join("Docs").unwrap();
    let file = docs.join("a.txt").unwrap();
    assert_eq!(file.to_string(), "Docs/a.txt");
    assert_eq!(file.parent(), Some(docs.clone()));
    assert_eq!(root.parent(), None);
    assert!(file.within(&docs));
    assert!(file.within(&RelPath::parse("docs").unwrap()));
    assert!(!docs.within(&file));
    assert!(!RelPath::parse("Docs2/a").unwrap().within(&docs));
    assert_eq!(root.join(".."), Err(PathError::Dots));
    assert_eq!(root.join("a/b"), Err(PathError::Separator));
    assert_eq!(root.join(".rcs-uploads"), Err(PathError::Internal));
    assert!(RelPath::parse(".Rcs-Trash/x").unwrap().in_trash());
    assert!(!RelPath::parse("x/.rcs-trash").unwrap().in_trash());
}

fn temp() -> PathBuf {
    crate::secrets::tests::temp_dir("files")
}

fn junction(link: &Path, target: &Path) -> bool {
    Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .is_ok_and(|o| o.status.success())
}

fn rel(text: &str) -> RelPath {
    RelPath::parse(text).unwrap()
}

#[test]
fn root_checks_at_start() {
    let dir = temp();
    assert!(Root::new(Path::new("relative"), None).is_err());
    assert!(Root::new(Path::new(r"C:\"), None).is_err());
    assert!(Root::new(&dir.join("missing"), None).is_err());
    std::fs::write(dir.join("file"), b"x").unwrap();
    assert!(Root::new(&dir.join("file"), None).is_err());
    let drive = dir.to_string_lossy().chars().next().unwrap();
    assert!(
        Root::new(&dir, Some(drive.to_ascii_lowercase()))
            .unwrap_err()
            .contains("системный")
    );
    let target = dir.join("real");
    std::fs::create_dir(&target).unwrap();
    if junction(&dir.join("linked"), &target) {
        assert!(
            Root::new(&dir.join("linked"), None)
                .unwrap_err()
                .contains("ссылка")
        );
    }
    let root = Root::new(&target, None).unwrap();
    assert!(target.join(TRASH).is_dir());
    assert!(target.join(UPLOADS).is_dir());
    // A second start finds both folders.
    Root::new(&target, None).unwrap();
    assert!(root.path().to_string_lossy().starts_with(r"\\?\"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn links_and_short_names_do_not_lead_out() {
    let dir = temp();
    let outside = dir.join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), b"secret").unwrap();
    let base = dir.join("root");
    std::fs::create_dir(&base).unwrap();
    let root = Root::new(&base, None).unwrap();
    std::fs::create_dir(base.join("Docs")).unwrap();
    std::fs::write(base.join("Docs").join("a.txt"), b"hello").unwrap();

    let o = root.open_file(&rel("Docs/a.txt")).unwrap();
    assert_eq!(o.meta.len(), 5);
    assert!(
        root.open_file(&rel("docs/A.TXT")).is_ok(),
        "case does not matter"
    );
    assert!(root.open_dir(&rel("")).is_ok());
    assert!(matches!(
        root.open_dir(&rel("Docs/a.txt")),
        Err(FsError::NotDir)
    ));
    assert!(matches!(root.open_file(&rel("Docs")), Err(FsError::IsDir)));
    assert!(matches!(
        root.open(&rel("Docs/missing")),
        Err(FsError::NotFound)
    ));
    assert!(matches!(
        root.open(&rel("Docs/a.txt/x")),
        Err(FsError::NotDir)
    ));

    assert!(
        junction(&base.join("out"), &outside),
        "mklink /J works without admin rights"
    );
    assert!(matches!(root.open(&rel("out")), Err(FsError::Link)));
    assert!(matches!(
        root.open_file(&rel("out/secret.txt")),
        Err(FsError::Link)
    ));
    assert!(matches!(
        root.vacant(&rel("out/new.txt")),
        Err(FsError::Link)
    ));
    assert!(junction(&base.join("Docs").join("deep"), &outside));
    assert!(matches!(
        root.open_file(&rel("Docs/deep/secret.txt")),
        Err(FsError::Link)
    ));

    // A file symbolic link needs a privilege or developer mode; checked when the machine allows creating one.
    if std::os::windows::fs::symlink_file(outside.join("secret.txt"), base.join("s.txt")).is_ok() {
        assert!(matches!(root.open_file(&rel("s.txt")), Err(FsError::Link)));
    }

    let long = base.join("A long folder name");
    std::fs::create_dir(&long).unwrap();
    std::fs::write(long.join("f.txt"), b"x").unwrap();
    if let Some(short) = short_name(&long) {
        assert!(
            matches!(
                root.open_file(&rel(&format!("{short}/f.txt"))),
                Err(FsError::Mismatch)
            ),
            "{short}"
        );
        assert!(matches!(
            root.open_dir(&rel(&short)),
            Err(FsError::Mismatch)
        ));
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn vacant_paths() {
    let dir = temp();
    let root = Root::new(&dir, None).unwrap();
    std::fs::write(dir.join("a.txt"), b"x").unwrap();
    assert_eq!(
        root.vacant(&rel("b.txt")).unwrap(),
        root.path().join("b.txt")
    );
    assert!(matches!(root.vacant(&rel("a.txt")), Err(FsError::Exists)));
    assert!(matches!(root.vacant(&rel("A.TXT")), Err(FsError::Exists)));
    assert!(matches!(root.vacant(&rel("")), Err(FsError::Exists)));
    assert!(matches!(
        root.vacant(&rel("missing/b.txt")),
        Err(FsError::NotFound)
    ));
    assert!(matches!(
        root.vacant(&rel("a.txt/b.txt")),
        Err(FsError::NotDir)
    ));
    let _ = std::fs::remove_dir_all(dir);
}

/// The 8.3 name Windows made for `path`, if the volume makes them.
fn short_name(path: &Path) -> Option<String> {
    use windows::Win32::Storage::FileSystem::GetShortPathNameW;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut buf = vec![0u16; 1024];
    // SAFETY: a NUL-terminated input and a writable buffer, both alive for the call.
    let n = unsafe { GetShortPathNameW(PCWSTR(wide.as_ptr()), Some(&mut buf)) } as usize;
    if n == 0 || n >= buf.len() {
        return None;
    }
    let short = String::from_utf16_lossy(&buf[..n]);
    let name = short.rsplit('\\').next()?.to_string();
    (name != "A long folder name").then_some(name)
}
