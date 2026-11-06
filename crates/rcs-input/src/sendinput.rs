//! `SendInput`: mouse, keyboard by scan code, wheel and text through `KEYEVENTF_UNICODE`.
//!
//! [`translate`] turns an event into [`Injection`]s and is pure, so tests check it without touching Windows.
//! [`SendInputSink`] sends them, and hands touch to [`TouchInjector`] (`InjectTouchInput`, P4-9).

use windows::Win32::Foundation::GetLastError;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE,
    MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    XBUTTON1, XBUTTON2,
};

use crate::touch::TouchInjector;
use crate::{Held, InputError, InputEvent, InputSink, MouseButton};

const SCAN_ENTER: u16 = 0x1c;
const SCAN_TAB: u16 = 0x0f;

/// Where the captured monitor sits on the Windows desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    /// The monitor: left, top, right, bottom in desktop pixels (`DXGI_OUTPUT_DESC::DesktopCoordinates`).
    pub monitor: [i32; 4],
    /// The virtual desktop over all monitors: left, top, width, height.
    pub desktop: [i32; 4],
}

impl Geometry {
    /// The virtual desktop as Windows reports it now.
    pub fn with_current_desktop(monitor: [i32; 4]) -> Self {
        // SAFETY: GetSystemMetrics takes no pointers.
        let desktop = unsafe {
            [
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            ]
        };
        Self { monitor, desktop }
    }

    /// The desktop pixel for a position normalized over the monitor.
    pub fn pixel(&self, x: u16, y: u16) -> (i32, i32) {
        let [l, t, r, b] = self.monitor;
        let along = |v: u16, from: i32, to: i32| {
            let span = i64::from((to - from - 1).max(0));
            from + ((i64::from(v) * span + 32_767) / 65_535) as i32
        };
        (along(x, l, r), along(y, t, b))
    }

    /// The `MOUSEEVENTF_VIRTUALDESK` coordinates that Windows maps back to `pixel` exactly: it takes
    /// `n * size / 65536`, so `n` is the smallest value at or above `pixel * 65536 / size`.
    pub fn normalized(&self, pixel: (i32, i32)) -> (i32, i32) {
        let [dl, dt, dw, dh] = self.desktop;
        let along = |p: i32, from: i32, size: i32| {
            let size = i64::from(size.max(1));
            let offset = i64::from((p - from).clamp(0, (size - 1) as i32));
            ((offset * 65_536 + size - 1) / size).min(65_535) as i32
        };
        (along(pixel.0, dl, dw), along(pixel.1, dt, dh))
    }
}

/// One `INPUT`, without the Windows types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Injection {
    Mouse {
        dx: i32,
        dy: i32,
        data: i32,
        flags: u32,
    },
    Key {
        scan: u16,
        flags: u32,
    },
}

fn key(scancode: u16, down: bool, out: &mut Vec<Injection>) {
    let mut flags = KEYEVENTF_SCANCODE.0;
    if scancode >> 8 == 0xe0 {
        flags |= KEYEVENTF_EXTENDEDKEY.0;
    }
    if !down {
        flags |= KEYEVENTF_KEYUP.0;
    }
    out.push(Injection::Key {
        scan: scancode & 0xff,
        flags,
    });
}

fn button(b: MouseButton, down: bool, out: &mut Vec<Injection>) {
    let (flags, data) = match (b, down) {
        (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
        (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
        (MouseButton::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1),
        (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
        (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2),
    };
    out.push(Injection::Mouse {
        dx: 0,
        dy: 0,
        data: i32::from(data),
        flags: flags.0,
    });
}

/// Appends the injections for one event. Touch adds nothing here.
pub fn translate(event: &InputEvent<'_>, geometry: &Geometry, out: &mut Vec<Injection>) {
    match *event {
        InputEvent::MouseAbsolute { x, y } => {
            let (dx, dy) = geometry.normalized(geometry.pixel(x, y));
            out.push(Injection::Mouse {
                dx,
                dy,
                data: 0,
                flags: (MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK).0,
            });
        }
        InputEvent::MouseRelative { dx, dy } => {
            if dx != 0 || dy != 0 {
                out.push(Injection::Mouse {
                    dx,
                    dy,
                    data: 0,
                    flags: MOUSEEVENTF_MOVE.0,
                });
            }
        }
        InputEvent::Button { button: b, down } => button(b, down, out),
        InputEvent::Wheel { dx, dy } => {
            if dy != 0 {
                out.push(Injection::Mouse {
                    dx: 0,
                    dy: 0,
                    data: i32::from(dy),
                    flags: MOUSEEVENTF_WHEEL.0,
                });
            }
            if dx != 0 {
                out.push(Injection::Mouse {
                    dx: 0,
                    dy: 0,
                    data: i32::from(dx),
                    flags: MOUSEEVENTF_HWHEEL.0,
                });
            }
        }
        InputEvent::Key { scancode, down } => key(scancode, down, out),
        InputEvent::Text(text) => {
            // Line breaks and tabs as keys: many programs ignore them as Unicode characters. "\r\n" is one Enter.
            let mut units = [0u16; 2];
            for c in text.chars() {
                match c {
                    '\r' => {}
                    '\n' | '\t' => {
                        let scan = if c == '\n' { SCAN_ENTER } else { SCAN_TAB };
                        key(scan, true, out);
                        key(scan, false, out);
                    }
                    _ => {
                        for &unit in c.encode_utf16(&mut units).iter() {
                            out.push(Injection::Key {
                                scan: unit,
                                flags: KEYEVENTF_UNICODE.0,
                            });
                            out.push(Injection::Key {
                                scan: unit,
                                flags: (KEYEVENTF_UNICODE | KEYEVENTF_KEYUP).0,
                            });
                        }
                    }
                }
            }
        }
        InputEvent::Touch(_) => {}
    }
}

/// The injections that let go of everything in `held`, which is emptied. Contacts are left to the touch sink.
pub fn release(held: &mut Held, out: &mut Vec<Injection>) {
    let (keys, buttons, _) = held.take();
    for k in keys {
        key(k, false, out);
    }
    for b in buttons {
        button(b, false, out);
    }
}

fn to_input(i: &Injection) -> INPUT {
    match *i {
        Injection::Mouse {
            dx,
            dy,
            data,
            flags,
        } => INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: data as u32,
                    dwFlags: MOUSE_EVENT_FLAGS(flags),
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
        Injection::Key { scan, flags } => INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: scan,
                    dwFlags: KEYBD_EVENT_FLAGS(flags),
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
    }
}

/// Sends input to the desktop of this session. Buffers are kept between events, so there is no allocation per move.
pub struct SendInputSink {
    geometry: Geometry,
    held: Held,
    plan: Vec<Injection>,
    inputs: Vec<INPUT>,
    /// Made on the first touch.
    touch: Option<TouchInjector>,
}

impl SendInputSink {
    pub fn new(geometry: Geometry) -> Self {
        Self {
            geometry,
            held: Held::default(),
            plan: Vec::with_capacity(64),
            inputs: Vec::with_capacity(64),
            touch: None,
        }
    }

    pub fn set_geometry(&mut self, geometry: Geometry) {
        self.geometry = geometry;
    }

    fn send(&mut self) -> Result<(), InputError> {
        if self.plan.is_empty() {
            return Ok(());
        }
        self.inputs.clear();
        self.inputs.extend(self.plan.iter().map(to_input));
        self.plan.clear();
        // SAFETY: the slice holds initialized INPUTs and the size is that of one INPUT, as the API requires.
        let sent = unsafe { SendInput(&self.inputs, size_of::<INPUT>() as i32) };
        let wanted = self.inputs.len() as u32;
        if sent == wanted {
            Ok(())
        } else {
            // SAFETY: no pointers; reads this thread's last error right after the call.
            let code = unsafe { GetLastError() }.0;
            Err(InputError::Blocked { sent, wanted, code })
        }
    }
}

impl InputSink for SendInputSink {
    fn handle(&mut self, event: &InputEvent<'_>) -> Result<(), InputError> {
        match *event {
            InputEvent::Key { scancode, down } => {
                self.held.key(scancode, down);
            }
            InputEvent::Button { button, down } => {
                self.held.button(button, down);
            }
            InputEvent::Touch(points) => {
                if self.touch.is_none() {
                    self.touch = Some(TouchInjector::new()?);
                }
                let geometry = self.geometry;
                return self
                    .touch
                    .as_mut()
                    .map_or(Ok(()), |t| t.touch(points, &geometry));
            }
            _ => {}
        }
        translate(event, &self.geometry, &mut self.plan);
        self.send()
    }

    fn release_all(&mut self) -> Result<(), InputError> {
        let touch = self.touch.as_mut().map_or(Ok(()), TouchInjector::release);
        release(&mut self.held, &mut self.plan);
        self.send().and(touch)
    }

    fn tick(&mut self) -> Result<(), InputError> {
        self.touch.as_mut().map_or(Ok(()), TouchInjector::tick)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_HD: Geometry = Geometry {
        monitor: [0, 0, 1920, 1080],
        desktop: [0, 0, 1920, 1080],
    };

    /// What Windows does with `MOUSEEVENTF_VIRTUALDESK` coordinates.
    fn windows_pixel(g: &Geometry, n: (i32, i32)) -> (i32, i32) {
        let [dl, dt, dw, dh] = g.desktop;
        (
            dl + ((i64::from(n.0) * i64::from(dw)) >> 16) as i32,
            dt + ((i64::from(n.1) * i64::from(dh)) >> 16) as i32,
        )
    }

    #[test]
    fn absolute_positions_land_on_the_right_pixel() {
        let two = Geometry {
            // The captured monitor is to the right of a 2560x1440 one and lower by 200 pixels.
            monitor: [2560, 200, 4480, 1280],
            desktop: [0, 0, 4480, 1440],
        };
        let left = Geometry {
            monitor: [-1920, 0, 0, 1080],
            desktop: [-1920, 0, 3840, 1080],
        };
        for g in [FULL_HD, two, left] {
            let [l, t, r, b] = g.monitor;
            assert_eq!(g.pixel(0, 0), (l, t));
            assert_eq!(g.pixel(65535, 65535), (r - 1, b - 1));
            for x in (0..=65535u16).step_by(97) {
                let p = g.pixel(x, x);
                assert!((l..r).contains(&p.0) && (t..b).contains(&p.1), "{p:?}");
                assert_eq!(windows_pixel(&g, g.normalized(p)), p, "{g:?} {x}");
            }
            // Every pixel of the monitor is reachable exactly.
            for px in l..r {
                assert_eq!(windows_pixel(&g, g.normalized((px, t))).0, px);
            }
        }
        assert_eq!(FULL_HD.pixel(32768, 32768), (960, 540));
    }

    #[test]
    fn keys_use_scan_codes() {
        let mut out = Vec::new();
        translate(
            &InputEvent::Key {
                scancode: 0x1e,
                down: true,
            },
            &FULL_HD,
            &mut out,
        );
        translate(
            &InputEvent::Key {
                scancode: 0xe01d,
                down: false,
            },
            &FULL_HD,
            &mut out,
        );
        assert_eq!(
            out,
            [
                Injection::Key {
                    scan: 0x1e,
                    flags: KEYEVENTF_SCANCODE.0
                },
                Injection::Key {
                    scan: 0x1d,
                    flags: (KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP).0
                },
            ]
        );
    }

    #[test]
    fn buttons_and_wheel() {
        let mut out = Vec::new();
        for (b, down) in [
            (MouseButton::Left, true),
            (MouseButton::Right, false),
            (MouseButton::Middle, true),
            (MouseButton::Back, true),
            (MouseButton::Forward, false),
        ] {
            translate(&InputEvent::Button { button: b, down }, &FULL_HD, &mut out);
        }
        translate(&InputEvent::Wheel { dx: 0, dy: -120 }, &FULL_HD, &mut out);
        translate(&InputEvent::Wheel { dx: 60, dy: 0 }, &FULL_HD, &mut out);
        translate(&InputEvent::Wheel { dx: 0, dy: 0 }, &FULL_HD, &mut out);
        let flags: Vec<(u32, i32)> = out
            .iter()
            .map(|i| match *i {
                Injection::Mouse { flags, data, .. } => (flags, data),
                Injection::Key { .. } => panic!("a key"),
            })
            .collect();
        assert_eq!(
            flags,
            [
                (MOUSEEVENTF_LEFTDOWN.0, 0),
                (MOUSEEVENTF_RIGHTUP.0, 0),
                (MOUSEEVENTF_MIDDLEDOWN.0, 0),
                (MOUSEEVENTF_XDOWN.0, i32::from(XBUTTON1)),
                (MOUSEEVENTF_XUP.0, i32::from(XBUTTON2)),
                (MOUSEEVENTF_WHEEL.0, -120),
                (MOUSEEVENTF_HWHEEL.0, 60),
            ]
        );
    }

    #[test]
    fn moves() {
        let mut out = Vec::new();
        translate(
            &InputEvent::MouseRelative { dx: -3, dy: 7 },
            &FULL_HD,
            &mut out,
        );
        translate(
            &InputEvent::MouseRelative { dx: 0, dy: 0 },
            &FULL_HD,
            &mut out,
        );
        translate(
            &InputEvent::MouseAbsolute { x: 0, y: 65535 },
            &FULL_HD,
            &mut out,
        );
        assert_eq!(
            out,
            [
                Injection::Mouse {
                    dx: -3,
                    dy: 7,
                    data: 0,
                    flags: MOUSEEVENTF_MOVE.0
                },
                Injection::Mouse {
                    dx: 0,
                    dy: FULL_HD.normalized((0, 1079)).1,
                    data: 0,
                    flags: (MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK).0
                },
            ]
        );
    }

    #[test]
    fn text_goes_as_unicode_with_keys_for_breaks() {
        let mut out = Vec::new();
        translate(&InputEvent::Text("я😀\r\n\t"), &FULL_HD, &mut out);
        let down = KEYEVENTF_UNICODE.0;
        let up = (KEYEVENTF_UNICODE | KEYEVENTF_KEYUP).0;
        let scan = KEYEVENTF_SCANCODE.0;
        let scan_up = (KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP).0;
        let k = |scan, flags| Injection::Key { scan, flags };
        assert_eq!(
            out,
            [
                k(0x044f, down),
                k(0x044f, up),
                k(0xd83d, down),
                k(0xd83d, up),
                k(0xde00, down),
                k(0xde00, up),
                k(SCAN_ENTER, scan),
                k(SCAN_ENTER, scan_up),
                k(SCAN_TAB, scan),
                k(SCAN_TAB, scan_up),
            ]
        );
    }

    #[test]
    fn release_lets_go_of_everything_held() {
        let mut held = Held::default();
        held.key(0x2a, true);
        held.key(0xe038, true);
        held.button(MouseButton::Left, true);
        let mut out = Vec::new();
        release(&mut held, &mut out);
        assert_eq!(
            out,
            [
                Injection::Key {
                    scan: 0x2a,
                    flags: (KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP).0
                },
                Injection::Key {
                    scan: 0x38,
                    flags: (KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP).0
                },
                Injection::Mouse {
                    dx: 0,
                    dy: 0,
                    data: 0,
                    flags: MOUSEEVENTF_LEFTUP.0
                },
            ]
        );
        assert!(held.is_empty());
        out.clear();
        release(&mut held, &mut out);
        assert!(out.is_empty(), "nothing held, nothing sent");
    }

    #[test]
    fn injections_become_inputs() {
        let i = to_input(&Injection::Key {
            scan: 0x1e,
            flags: KEYEVENTF_SCANCODE.0,
        });
        assert_eq!(i.r#type, INPUT_KEYBOARD);
        // SAFETY: the union holds the keyboard variant written just above.
        let ki = unsafe { i.Anonymous.ki };
        assert_eq!((ki.wScan, ki.dwFlags), (0x1e, KEYEVENTF_SCANCODE));
        let i = to_input(&Injection::Mouse {
            dx: 0,
            dy: 0,
            data: -120,
            flags: MOUSEEVENTF_WHEEL.0,
        });
        assert_eq!(i.r#type, INPUT_MOUSE);
        // SAFETY: the union holds the mouse variant written just above.
        let mi = unsafe { i.Anonymous.mi };
        assert_eq!(mi.mouseData as i32, -120);
    }
}
