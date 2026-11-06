//! Paths of the file API (docs/SECURITY.md «Файлы», D-026).
//!
//! A request path is parsed into checked names first, without touching the disk. Resolving it then refuses every
//! reparse point on the way, opens the object itself without following one and compares the path Windows reports for
//! the open handle with the expected one: 8.3 short names, links and a swap between the check and the open all show
//! up as a mismatch.

use std::ffi::OsString;
use std::fmt;
use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Component, Path, PathBuf, Prefix};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_NAME_NORMALIZED,
    GETFINALPATHNAMEBYHANDLE_FLAGS, GetDriveTypeW, GetFinalPathNameByHandleW, VOLUME_NAME_DOS,
};
use windows::core::PCWSTR;

/// Where deleted objects go; visible, but nothing is deleted in it or moved into it through the API.
pub const TRASH: &str = ".rcs-trash";
/// Unfinished uploads; not reachable through the API at all.
pub const UPLOADS: &str = ".rcs-uploads";
pub const MAX_PATH_BYTES: usize = 4096;
/// NTFS limit for one name, in UTF-16 code units.
pub const MAX_NAME_UNITS: usize = 255;

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const DRIVE_REMOTE: u32 = 4;

/// Why a request path was refused before the disk was touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathError {
    TooLong,
    Absolute,
    Empty,
    Dots,
    Separator,
    Stream,
    Character,
    TrailingDotOrSpace,
    Reserved,
    Internal,
}

impl PathError {
    pub fn code(self) -> &'static str {
        match self {
            Self::TooLong => "path_too_long",
            Self::Absolute => "path_absolute",
            Self::Empty => "path_empty_name",
            Self::Dots => "path_dots",
            Self::Separator => "path_backslash",
            Self::Stream => "path_stream",
            Self::Character => "path_character",
            Self::TrailingDotOrSpace => "path_trailing_dot",
            Self::Reserved => "path_reserved",
            Self::Internal => "path_internal",
        }
    }
}

/// A checked path relative to the root: zero names for the root itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelPath {
    names: Vec<String>,
}

impl RelPath {
    pub fn root() -> Self {
        Self { names: Vec::new() }
    }

    pub fn parse(text: &str) -> Result<Self, PathError> {
        if text.len() > MAX_PATH_BYTES {
            return Err(PathError::TooLong);
        }
        if text.is_empty() {
            return Ok(Self::root());
        }
        if text.starts_with('/') {
            return Err(PathError::Absolute);
        }
        let names = text
            .split('/')
            .map(|n| check_name(n).map(|()| n.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        if names[0].eq_ignore_ascii_case(UPLOADS) {
            return Err(PathError::Internal);
        }
        Ok(Self { names })
    }

    pub fn names(&self) -> &[String] {
        &self.names
    }

    pub fn is_root(&self) -> bool {
        self.names.is_empty()
    }

    pub fn name(&self) -> Option<&str> {
        self.names.last().map(String::as_str)
    }

    pub fn parent(&self) -> Option<Self> {
        let (_, rest) = self.names.split_last()?;
        Some(Self {
            names: rest.to_vec(),
        })
    }

    /// `name` must already be a checked name, e.g. one of `names()` of another path.
    pub fn join(&self, name: &str) -> Result<Self, PathError> {
        check_name(name)?;
        let mut names = self.names.clone();
        names.push(name.to_string());
        let path = Self { names };
        if path.to_string().len() > MAX_PATH_BYTES {
            return Err(PathError::TooLong);
        }
        if path.names[0].eq_ignore_ascii_case(UPLOADS) {
            return Err(PathError::Internal);
        }
        Ok(path)
    }

    /// The trash folder itself or anything in it.
    pub fn in_trash(&self) -> bool {
        self.names
            .first()
            .is_some_and(|n| n.eq_ignore_ascii_case(TRASH))
    }

    /// `self` is `other` or inside it, compared the way NTFS compares names.
    pub fn within(&self, other: &Self) -> bool {
        other.names.len() <= self.names.len()
            && other
                .names
                .iter()
                .zip(&self.names)
                .all(|(a, b)| same_name(a, b))
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.names.join("/"))
    }
}

/// Case-insensitive, as NTFS treats names. Lowercasing may differ from the volume's upcase table for a few letters;
/// a difference only makes a check fail, never pass.
pub fn same_name(a: &str, b: &str) -> bool {
    a == b || a.to_lowercase() == b.to_lowercase()
}

/// One name: a file or folder name as NTFS stores it, with nothing Windows would reinterpret.
pub fn check_name(name: &str) -> Result<(), PathError> {
    if name.is_empty() {
        return Err(PathError::Empty);
    }
    if name == "." || name == ".." {
        return Err(PathError::Dots);
    }
    for c in name.chars() {
        match c {
            '\\' | '/' => return Err(PathError::Separator),
            ':' => return Err(PathError::Stream),
            '<' | '>' | '"' | '|' | '?' | '*' => return Err(PathError::Character),
            c if c.is_control() => return Err(PathError::Character),
            _ => {}
        }
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(PathError::TrailingDotOrSpace);
    }
    if name.encode_utf16().count() > MAX_NAME_UNITS {
        return Err(PathError::TooLong);
    }
    if is_reserved(name) {
        return Err(PathError::Reserved);
    }
    Ok(())
}

/// DOS device names, also with an extension (`nul.txt`) and spaces before it (`CON .txt`).
fn is_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    let upper = stem.to_uppercase();
    if matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) {
        return true;
    }
    let mut chars = upper.chars();
    let head: String = chars.by_ref().take(3).collect();
    let tail: Vec<char> = chars.collect();
    (head == "COM" || head == "LPT")
        && matches!(tail.as_slice(), [d] if d.is_ascii_digit() || matches!(d, '¹' | '²' | '³'))
}

/// Why resolving a checked path failed.
#[derive(Debug)]
pub enum FsError {
    NotFound,
    Exists,
    NotDir,
    IsDir,
    Link,
    /// The path Windows reports differs from the requested one (a short name, a swapped component).
    Mismatch,
    /// Allowed path, refused operation (the trash rules).
    Forbidden,
    Io(io::Error),
}

impl From<io::Error> for FsError {
    fn from(e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::NotFound => Self::NotFound,
            io::ErrorKind::AlreadyExists => Self::Exists,
            _ => Self::Io(e),
        }
    }
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            other => write!(f, "{other:?}"),
        }
    }
}

pub fn is_link(meta: &Metadata) -> bool {
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// An existing object, opened without following a reparse point and checked.
pub struct Opened {
    pub file: File,
    pub meta: Metadata,
    pub path: PathBuf,
}

/// The checked `files.root`.
#[derive(Debug, Clone)]
pub struct Root {
    /// Canonical, in the `\\?\D:\…` form, so long paths work and Windows rewrites nothing.
    path: PathBuf,
}

impl Root {
    /// The start-up check (D-026). `system_drive` is the letter that is refused, `None` only in tests.
    pub fn new(dir: &Path, system_drive: Option<char>) -> Result<Self, String> {
        if !dir.is_absolute() {
            return Err(format!("{}: нужен абсолютный путь", dir.display()));
        }
        let meta = std::fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        if is_link(&meta) {
            return Err(format!("{}: ссылка или соединение", dir.display()));
        }
        if !meta.is_dir() {
            return Err(format!("{}: не папка", dir.display()));
        }
        let path = std::fs::canonicalize(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let mut components = path.components();
        let letter = match components.next() {
            Some(Component::Prefix(p)) => match p.kind() {
                Prefix::VerbatimDisk(d) | Prefix::Disk(d) => char::from(d).to_ascii_uppercase(),
                _ => return Err(format!("{}: сетевой путь", path.display())),
            },
            _ => return Err(format!("{}: нет буквы диска", path.display())),
        };
        if !matches!(components.next(), Some(Component::RootDir)) || components.next().is_none() {
            return Err(format!("{}: корень диска", path.display()));
        }
        if system_drive.is_some_and(|s| s.eq_ignore_ascii_case(&letter)) {
            return Err(format!("{}: системный диск", path.display()));
        }
        let drive: Vec<u16> = format!("{letter}:\\").encode_utf16().chain([0]).collect();
        // SAFETY: a NUL-terminated string that lives for the call.
        if unsafe { GetDriveTypeW(PCWSTR(drive.as_ptr())) } == DRIVE_REMOTE {
            return Err(format!("{}: сетевой диск", path.display()));
        }
        let root = Self { path };
        for name in [TRASH, UPLOADS] {
            let dir = root.path.join(name);
            match std::fs::symlink_metadata(&dir) {
                Ok(m) if is_link(&m) || !m.is_dir() => {
                    return Err(format!("{}: не папка", dir.display()));
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    std::fs::create_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
                }
                Err(e) => return Err(format!("{}: {e}", dir.display())),
            }
        }
        Ok(root)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Where `rel` would be; nothing is checked on disk.
    pub fn full(&self, rel: &RelPath) -> PathBuf {
        let mut p = self.path.clone();
        for name in rel.names() {
            p.push(name);
        }
        p
    }

    /// The uploads folder, for the upload code only.
    pub fn uploads(&self) -> PathBuf {
        self.path.join(UPLOADS)
    }

    /// Every existing parent of `rel` must be a real folder, not a reparse point.
    fn check_parents(&self, rel: &RelPath) -> Result<(), FsError> {
        let mut p = self.path.clone();
        let names = rel.names();
        for name in names.iter().take(names.len().saturating_sub(1)) {
            p.push(name);
            let meta = std::fs::symlink_metadata(&p)?;
            if is_link(&meta) {
                return Err(FsError::Link);
            }
            if !meta.is_dir() {
                return Err(FsError::NotDir);
            }
        }
        Ok(())
    }

    /// Opens an existing object for reading. Folders open too (`FILE_FLAG_BACKUP_SEMANTICS`).
    pub fn open(&self, rel: &RelPath) -> Result<Opened, FsError> {
        self.check_parents(rel)?;
        let path = self.full(rel);
        let meta = std::fs::symlink_metadata(&path)?;
        if is_link(&meta) {
            return Err(FsError::Link);
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(&path)?;
        let meta = file.metadata()?;
        if is_link(&meta) {
            return Err(FsError::Link);
        }
        if !same_path(&final_path(&file)?, &path) {
            return Err(FsError::Mismatch);
        }
        Ok(Opened { file, meta, path })
    }

    pub fn open_dir(&self, rel: &RelPath) -> Result<Opened, FsError> {
        let o = self.open(rel)?;
        if o.meta.is_dir() {
            Ok(o)
        } else {
            Err(FsError::NotDir)
        }
    }

    pub fn open_file(&self, rel: &RelPath) -> Result<Opened, FsError> {
        let o = self.open(rel)?;
        if o.meta.is_dir() {
            Err(FsError::IsDir)
        } else {
            Ok(o)
        }
    }

    /// A path for a new object: the parent is a checked folder and nothing exists at `rel` yet. Creating it is the
    /// caller's job, with an operation that fails if something appeared meanwhile.
    pub fn vacant(&self, rel: &RelPath) -> Result<PathBuf, FsError> {
        let parent = rel.parent().ok_or(FsError::Exists)?;
        self.open_dir(&parent)?;
        let path = self.full(rel);
        match std::fs::symlink_metadata(&path) {
            Ok(_) => Err(FsError::Exists),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(path),
            Err(e) => Err(e.into()),
        }
    }
}

/// Paths in the `\\?\` form compared case-insensitively.
fn same_path(a: &Path, b: &Path) -> bool {
    let wide =
        |p: &Path| String::from_utf16_lossy(&p.as_os_str().encode_wide().collect::<Vec<_>>());
    same_name(&wide(a), &wide(b))
}

/// The path Windows reports for an open handle, `\\?\D:\…`.
fn final_path(file: &File) -> io::Result<PathBuf> {
    let handle = HANDLE(file.as_raw_handle());
    let mut buf = vec![0u16; 512];
    loop {
        // SAFETY: the handle stays open for the call and the buffer is writable for its whole length.
        let n = unsafe {
            GetFinalPathNameByHandleW(
                handle,
                &mut buf,
                GETFINALPATHNAMEBYHANDLE_FLAGS(FILE_NAME_NORMALIZED.0 | VOLUME_NAME_DOS.0),
            )
        };
        let n = n as usize;
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        if n < buf.len() {
            buf.truncate(n);
            return Ok(PathBuf::from(OsString::from_wide(&buf)));
        }
        buf.resize(n + 1, 0);
    }
}

#[cfg(test)]
mod tests;
