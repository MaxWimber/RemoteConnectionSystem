//! The desktop that takes input: `Default`, or `Winlogon` for the sign-in screen, the lock screen and UAC prompts
//! (P6-4). Capture and input threads move to it with `SetThreadDesktop`; only SYSTEM may open `Winlogon`, so as an
//! ordinary user the move to it fails and the thread stays where it is.

use std::cell::RefCell;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetUserObjectInformationW, HDESK,
    OpenInputDesktop, SetThreadDesktop, UOI_NAME,
};
use windows::core::Error;

/// GENERIC_ALL: reading the screen and sending input both need it on the secure desktop.
const GENERIC_ALL: u32 = 0x1000_0000;

thread_local! {
    /// The desktop this thread was moved to, closed when it moves again.
    static ATTACHED: RefCell<Option<(HDESK, String)>> = const { RefCell::new(None) };
}

fn name_of(desktop: HDESK) -> String {
    let mut buf = [0u16; 64];
    let mut needed = 0u32;
    // SAFETY: the buffer and its size in bytes are given; the out pointer refers to a live local.
    let ok = unsafe {
        GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(buf.as_mut_ptr().cast()),
            (buf.len() * 2) as u32,
            Some(&mut needed),
        )
    };
    if ok.is_err() {
        return String::new();
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

fn open_input() -> Result<HDESK, Error> {
    // SAFETY: no pointers; the handle is closed by the caller.
    unsafe {
        OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(GENERIC_ALL),
        )
    }
}

/// The name of the desktop that takes input now, if this process may open it.
pub fn input_desktop_name() -> Option<String> {
    let desktop = open_input().ok()?;
    let name = name_of(desktop);
    // SAFETY: the handle was opened just above and is closed once.
    let _ = unsafe { CloseDesktop(desktop) };
    Some(name)
}

/// The name of the desktop this thread was last moved to by [`follow_input_desktop`].
pub fn attached_name() -> Option<String> {
    ATTACHED.with(|a| a.borrow().as_ref().map(|(_, n)| n.clone()))
}

/// Moves the calling thread to the input desktop if it is not there yet. Returns the desktop's name when the thread
/// moved. The thread must own no windows or hooks, which capture and input threads do not.
pub fn follow_input_desktop() -> Result<Option<String>, Error> {
    let desktop = open_input()?;
    let name = name_of(desktop);
    if attached_name().as_deref() == Some(name.as_str()) {
        // SAFETY: the handle was opened just above and is closed once.
        let _ = unsafe { CloseDesktop(desktop) };
        return Ok(None);
    }
    // SAFETY: the handle is a live desktop of this window station.
    if let Err(e) = unsafe { SetThreadDesktop(desktop) } {
        // SAFETY: the handle was opened above and is not in use.
        let _ = unsafe { CloseDesktop(desktop) };
        return Err(e);
    }
    let previous = ATTACHED.with(|a| a.borrow_mut().replace((desktop, name.clone())));
    if let Some((old, _)) = previous {
        // SAFETY: the thread no longer uses the old handle; it is closed once.
        let _ = unsafe { CloseDesktop(old) };
    }
    Ok(Some(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ordinary_user_sees_the_default_desktop() {
        // The sign-in screen would be Winlogon, but tests run in a signed-in session.
        assert_eq!(input_desktop_name().as_deref(), Some("Default"));
        assert_eq!(follow_input_desktop().unwrap().as_deref(), Some("Default"));
        assert_eq!(follow_input_desktop().unwrap(), None, "already there");
        assert_eq!(attached_name().as_deref(), Some("Default"));
    }
}
