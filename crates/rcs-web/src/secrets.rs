//! The sign-in secrets in `%ProgramData%\RCS\secrets\auth.json` (docs/SECURITY.md, D-021).
//!
//! The directory gets a protected DACL: SYSTEM, Administrators and the account `rcs-web` runs under. The TOTP
//! secret is encrypted with DPAPI in the machine context; recovery codes are stored as SHA-256.

use std::path::{Path, PathBuf};

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_LOCAL_MACHINE, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData,
    CryptUnprotectData,
};
use windows::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows::Win32::Security::{LookupAccountNameW, PSID, SID_NAME_USE};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{BOOL, HSTRING, PCWSTR, PWSTR, w};

use crate::util::{hex, sha256, unhex};

pub const MIN_PASSWORD_CHARS: usize = 16;
pub const MAX_PASSWORD_BYTES: usize = 1024;
pub const RECOVERY_CODES: usize = 8;
const FILE_NAME: &str = "auth.json";

/// OWASP minimum for Argon2id (docs/SECURITY.md).
const ARGON_M_KIB: u32 = 19_456;
const ARGON_T: u32 = 2;
const ARGON_P: u32 = 1;

pub fn default_dir() -> PathBuf {
    let base = std::env::var_os("ProgramData")
        .map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from);
    base.join("RCS").join("secrets")
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AuthFile {
    #[serde(default)]
    pub password_hash: Option<String>,
    /// DPAPI blob of the TOTP secret, hex.
    #[serde(default)]
    pub totp_dpapi: Option<String>,
    /// SHA-256 of each unused recovery code, hex.
    #[serde(default)]
    pub recovery_sha256: Vec<String>,
    /// The last TOTP step that signed someone in; older and equal steps are refused.
    #[serde(default)]
    pub totp_last_step: Option<u64>,
}

impl AuthFile {
    #[cfg(test)]
    pub fn is_complete(&self) -> bool {
        self.password_hash.is_some() && self.totp_dpapi.is_some()
    }

    pub fn totp_secret(&self) -> Result<Vec<u8>, String> {
        let blob = self
            .totp_dpapi
            .as_deref()
            .ok_or("TOTP не настроен: выполните rcs-web enroll-totp")?;
        let blob = unhex(blob).ok_or("секрет TOTP повреждён")?;
        unprotect(&blob)
    }

    pub fn set_totp_secret(&mut self, secret: &[u8]) -> Result<(), String> {
        self.totp_dpapi = Some(hex(&protect(secret)?));
        Ok(())
    }

    pub fn set_recovery_codes(&mut self, codes: &[String]) {
        self.recovery_sha256 = codes
            .iter()
            .map(|c| hex(&sha256(normalize_recovery(c).as_bytes())))
            .collect();
    }

    /// Removes a matching recovery code. Every stored hash is compared, in constant time each.
    pub fn take_recovery_code(&mut self, code: &str) -> bool {
        let wanted = sha256(normalize_recovery(code).as_bytes());
        let mut found = None;
        for (i, stored) in self.recovery_sha256.iter().enumerate() {
            if let Some(stored) = unhex(stored)
                && bool::from(stored.ct_eq(&wanted))
            {
                found = Some(i);
            }
        }
        match found {
            Some(i) => {
                self.recovery_sha256.remove(i);
                true
            }
            None => false,
        }
    }
}

/// Recovery codes are shown as `ABCD-EFGH-JKLM-NPQR`; case, spaces and dashes do not matter on input.
pub fn normalize_recovery(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// A new recovery code: 80 random bits in base32, four groups of four.
pub fn new_recovery_code() -> String {
    let text = crate::totp::base32(&crate::util::random::<10>());
    text.as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("-")
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self) -> PathBuf {
        self.dir.join(FILE_NAME)
    }

    /// Creates the directory if needed and sets its DACL; a directory someone else controls fails here.
    pub fn prepare(&self) -> Result<(), String> {
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("{}: {e}", self.dir.display()))?;
        restrict_dir(&self.dir)
    }

    pub fn load(&self) -> Result<AuthFile, String> {
        match std::fs::read(self.path()) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| format!("{}: {e}", self.path().display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AuthFile::default()),
            Err(e) => Err(format!("{}: {e}", self.path().display())),
        }
    }

    /// Writes the whole file through a temporary one, so a crash leaves the old or the new version.
    pub fn save(&self, file: &AuthFile) -> Result<(), String> {
        let temp = self.dir.join(format!("{FILE_NAME}.tmp"));
        let bytes = serde_json::to_vec_pretty(file).map_err(|e| e.to_string())?;
        std::fs::write(&temp, bytes).map_err(|e| format!("{}: {e}", temp.display()))?;
        std::fs::rename(&temp, self.path()).map_err(|e| format!("{}: {e}", self.path().display()))
    }
}

fn argon() -> Argon2<'static> {
    let params = Params::new(ARGON_M_KIB, ARGON_T, ARGON_P, None).expect("valid Argon2 parameters");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

pub fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::encode_b64(&crate::util::random::<16>()).map_err(|e| e.to_string())?;
    argon()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

/// Checks the stored hash is Argon2id with at least the required parameters.
pub fn check_hash(stored: &str) -> Result<(), String> {
    let parsed = PasswordHash::new(stored).map_err(|_| "хэш пароля повреждён")?;
    let params = Params::try_from(&parsed).map_err(|_| "хэш пароля повреждён")?;
    if parsed.algorithm != Algorithm::Argon2id.ident()
        || params.m_cost() < ARGON_M_KIB
        || params.t_cost() < ARGON_T
        || params.p_cost() < ARGON_P
    {
        return Err(
            "хэш пароля слабее требований: задайте пароль заново через rcs-web set-password".into(),
        );
    }
    Ok(())
}

/// Argon2 does the same work whatever the password, so the time does not depend on a correct prefix.
pub fn verify_password(stored: &str, password: &str) -> bool {
    if password.len() > MAX_PASSWORD_BYTES {
        return false;
    }
    let Ok(parsed) = PasswordHash::new(stored) else {
        return false;
    };
    argon()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

fn protect(data: &[u8]) -> Result<Vec<u8>, String> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(data.len()).map_err(|_| "too much data")?,
        pbData: data.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: `input` points into `data`, which outlives the call, and DPAPI only reads it. `output` receives a
    // LocalAlloc buffer that `take_blob` copies and frees.
    unsafe {
        CryptProtectData(
            &input,
            w!("rcs-web totp"),
            None,
            None,
            None,
            CRYPTPROTECT_LOCAL_MACHINE | CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    }
    .map_err(|e| format!("DPAPI: {e}"))?;
    Ok(take_blob(output))
}

fn unprotect(blob: &[u8]) -> Result<Vec<u8>, String> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(blob.len()).map_err(|_| "too much data")?,
        pbData: blob.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: as in `protect`; the description is not requested.
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    }
    .map_err(|e| format!("DPAPI: {e}"))?;
    Ok(take_blob(output))
}

fn take_blob(blob: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    if blob.pbData.is_null() {
        return Vec::new();
    }
    // SAFETY: DPAPI returned `cbData` bytes at `pbData`; they are copied before the buffer is freed once.
    unsafe {
        let bytes = std::slice::from_raw_parts(blob.pbData, blob.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(blob.pbData.cast())));
        bytes
    }
}

/// The account `rcs-web` runs under as a service (D-025). It exists only once the service is installed.
pub const SERVICE_ACCOUNT: &str = r"NT SERVICE\rcs-web";

/// The SID of an account as `S-1-5-...`, or None when the account does not exist.
pub fn account_sid(name: &str) -> Option<String> {
    let name = HSTRING::from(name);
    let mut sid = vec![0u8; 128];
    let mut sid_len = sid.len() as u32;
    let mut domain = vec![0u16; 256];
    let mut domain_len = domain.len() as u32;
    let mut kind = SID_NAME_USE::default();
    // SAFETY: both buffers are writable for the lengths passed; the out pointers refer to live locals.
    unsafe {
        LookupAccountNameW(
            PCWSTR::null(),
            &name,
            Some(PSID(sid.as_mut_ptr().cast())),
            &mut sid_len,
            Some(PWSTR(domain.as_mut_ptr())),
            &mut domain_len,
            &mut kind,
        )
    }
    .ok()?;
    let mut text = PWSTR::null();
    // SAFETY: the SID lives in `sid`; `text` is a LocalAlloc string freed below.
    unsafe {
        ConvertSidToStringSidW(PSID(sid.as_mut_ptr().cast()), &mut text).ok()?;
        let out = text.to_string().ok();
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        out
    }
}

/// The SID of the account this process runs under, as `S-1-5-21-...`.
fn current_user_sid() -> Result<String, String> {
    let mut token = HANDLE::default();
    // SAFETY: the pseudo handle of the current process needs no closing; `token` receives a handle closed below.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(|e| e.to_string())?;
    let mut buffer = vec![0u8; 256];
    let mut needed = 0u32;
    // SAFETY: `buffer` is writable for its length, which is passed; the token is valid until CloseHandle.
    let result = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            buffer.len() as u32,
            &mut needed,
        )
    };
    // SAFETY: `token` came from OpenProcessToken above and is closed once.
    unsafe {
        let _ = CloseHandle(token);
    }
    result.map_err(|e| e.to_string())?;
    // SAFETY: on success the buffer starts with a TOKEN_USER whose SID points inside the same buffer.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut text = PWSTR::null();
    // SAFETY: the SID is valid while `buffer` lives; `text` is a LocalAlloc string freed below.
    unsafe {
        ConvertSidToStringSidW(user.User.Sid, &mut text).map_err(|e| e.to_string())?;
        let sid = text.to_string().map_err(|e| e.to_string());
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        sid
    }
}

/// Protected DACL: full control for SYSTEM, Administrators, the current account and, once installed, the
/// `rcs-web` service account, inherited by the files. So `set-password` run by an administrator keeps the service
/// able to read the secrets.
fn restrict_dir(dir: &Path) -> Result<(), String> {
    let sid = current_user_sid()?;
    let service = account_sid(SERVICE_ACCOUNT)
        .filter(|s| *s != sid)
        .map(|s| format!("(A;OICI;FA;;;{s})"))
        .unwrap_or_default();
    let sddl = HSTRING::from(format!(
        "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{sid}){service}"
    ));
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `sddl` is a valid wide string; `descriptor` receives a LocalAlloc block freed below.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &sddl,
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
    }
    .map_err(|e| e.to_string())?;
    let mut present = BOOL(0);
    let mut defaulted = BOOL(0);
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let path = HSTRING::from(dir.as_os_str());
    // SAFETY: the descriptor is valid until LocalFree; the DACL pointer points inside it and is used before that.
    let result = unsafe {
        GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted)
            .map_err(|e| e.to_string())
            .and_then(|()| {
                SetNamedSecurityInfoW(
                    &path,
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    None,
                    None,
                    Some(dacl),
                    None,
                )
                .ok()
                .map_err(|e| format!("{}: {e}", dir.display()))
            })
    };
    // SAFETY: `descriptor` was allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }
    result
}

#[cfg(test)]
pub mod tests {
    #[test]
    fn accounts_resolve_to_sids() {
        assert_eq!(
            super::account_sid(r"NT AUTHORITY\SYSTEM").as_deref(),
            Some("S-1-5-18")
        );
        assert_eq!(super::account_sid(r"NT SERVICE\no-such-service-rcs"), None);
    }

    use super::*;

    /// A fresh directory under the system temp folder for one test.
    pub fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rcs-web-test-{name}-{}",
            hex(&crate::util::random::<6>())
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn dpapi_round_trip() {
        let mut file = AuthFile::default();
        file.set_totp_secret(b"0123456789abcdefghij").unwrap();
        assert_ne!(
            file.totp_dpapi.as_deref(),
            Some(hex(b"0123456789abcdefghij").as_str())
        );
        assert_eq!(file.totp_secret().unwrap(), b"0123456789abcdefghij");
    }

    #[test]
    fn password_hash_meets_the_minimum() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(
            hash.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
            "{hash}"
        );
        check_hash(&hash).unwrap();
        assert!(verify_password(&hash, "correct horse battery staple"));
        assert!(!verify_password(&hash, "correct horse battery stapl"));
        let weak = "$argon2id$v=19$m=1024,t=1,p=1$c29tZXNhbHQ$RdescudvJCsgt3ub+b+dWRWJTmaaJObG";
        assert!(check_hash(weak).is_err());
    }

    #[test]
    fn recovery_codes_work_once() {
        let codes: Vec<String> = (0..RECOVERY_CODES).map(|_| new_recovery_code()).collect();
        assert_eq!(codes[0].len(), 19);
        let mut file = AuthFile::default();
        file.set_recovery_codes(&codes);
        let typed = codes[3].to_lowercase().replace('-', " ");
        assert!(file.take_recovery_code(&typed));
        assert!(!file.take_recovery_code(&codes[3]));
        assert_eq!(file.recovery_sha256.len(), RECOVERY_CODES - 1);
    }

    #[test]
    fn store_round_trip_with_private_dir() {
        let dir = temp_dir("store").join("secrets");
        let store = Store::new(dir.clone());
        store.prepare().unwrap();
        assert!(!store.load().unwrap().is_complete());
        let file = AuthFile {
            password_hash: Some("x".into()),
            totp_dpapi: Some("00".into()),
            totp_last_step: Some(7),
            ..AuthFile::default()
        };
        store.save(&file).unwrap();
        let loaded = store.load().unwrap();
        assert!(loaded.is_complete());
        assert_eq!(loaded.totp_last_step, Some(7));
        // Only the full-control entries remain, none of them inherited: SYSTEM, Administrators, this account, and
        // the rcs-web service account once the service is installed (phase 6).
        let acl = std::process::Command::new("icacls")
            .arg(&dir)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&acl.stdout).to_string();
        let expected = 3 + usize::from(account_sid(SERVICE_ACCOUNT).is_some());
        assert_eq!(text.matches("(OI)(CI)(F)").count(), expected, "{text}");
        assert!(!text.contains("(I)") && !text.contains("(RX)"), "{text}");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }
}
