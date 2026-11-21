//! Interactive-input plumbing for the native viewport (Plan.Md Â§5 M4B):
//! winitâ†’evdev key mapping, the non-blocking control pump, Win32 clipboard
//! bridging, and the low-level keyboard grab (WH_KEYBOARD_LL).
//!
//! Threading rules (the reason this module exists): the winit UI thread must
//! NEVER touch the control socket. All outbound `InputEvent`/`Resize`/
//! `ClipboardUpdate`/`StopSession` traffic goes through a bounded mpsc into a
//! dedicated writer thread; a reader thread decodes inbound frames and
//! hands `ClipboardUpdate`/`SessionStopped` back through a shared slot.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use common::keymap;
use common::protocol::ControlMessage;
use crate::transport::{self, HvConnection};

// winit 0.30 naming notes: the Win key is `SuperLeft`/`SuperRight`, media
// keys are `AudioVolume*` â€” mapped below to their evdev equivalents.

// ---------------------------------------------------------------------------
// winit â†’ evdev mapping (host side; common/keymap.rs holds the codes)
// ---------------------------------------------------------------------------

/// Map `winit::keyboard::KeyCode` (physical key position) to a Linux evdev
/// keycode. Positions map 1:1 for US-QWERTY-identical layouts; letters use
/// unshifted positions.
pub fn winit_key_to_evdev(code: winit::keyboard::KeyCode) -> Option<u32> {
    use winit::keyboard::KeyCode as K;
    Some(match code {
        K::Escape => keymap::KEY_ESC,
        K::Backquote => keymap::KEY_GRAVE,
        K::Digit1 => keymap::KEY_1,
        K::Digit2 => keymap::KEY_2,
        K::Digit3 => keymap::KEY_3,
        K::Digit4 => keymap::KEY_4,
        K::Digit5 => keymap::KEY_5,
        K::Digit6 => keymap::KEY_6,
        K::Digit7 => keymap::KEY_7,
        K::Digit8 => keymap::KEY_8,
        K::Digit9 => keymap::KEY_9,
        K::Digit0 => keymap::KEY_0,
        K::Minus => keymap::KEY_MINUS,
        K::Equal => keymap::KEY_EQUAL,
        K::Backspace => keymap::KEY_BACKSPACE,
        K::Tab => keymap::KEY_TAB,
        K::KeyQ => keymap::KEY_Q,
        K::KeyW => keymap::KEY_W,
        K::KeyE => keymap::KEY_E,
        K::KeyR => keymap::KEY_R,
        K::KeyT => keymap::KEY_T,
        K::KeyY => keymap::KEY_Y,
        K::KeyU => keymap::KEY_U,
        K::KeyI => keymap::KEY_I,
        K::KeyO => keymap::KEY_O,
        K::KeyP => keymap::KEY_P,
        K::BracketLeft => keymap::KEY_LEFTBRACE,
        K::BracketRight => keymap::KEY_RIGHTBRACE,
        K::Enter => keymap::KEY_ENTER,
        K::ControlLeft => keymap::KEY_LEFTCTRL,
        K::KeyA => keymap::KEY_A,
        K::KeyS => keymap::KEY_S,
        K::KeyD => keymap::KEY_D,
        K::KeyF => keymap::KEY_F,
        K::KeyG => keymap::KEY_G,
        K::KeyH => keymap::KEY_H,
        K::KeyJ => keymap::KEY_J,
        K::KeyK => keymap::KEY_K,
        K::KeyL => keymap::KEY_L,
        K::Semicolon => keymap::KEY_SEMICOLON,
        K::Quote => keymap::KEY_APOSTROPHE,
        K::ShiftLeft => keymap::KEY_LEFTSHIFT,
        K::Backslash => keymap::KEY_BACKSLASH,
        K::KeyZ => keymap::KEY_Z,
        K::KeyX => keymap::KEY_X,
        K::KeyC => keymap::KEY_C,
        K::KeyV => keymap::KEY_V,
        K::KeyB => keymap::KEY_B,
        K::KeyN => keymap::KEY_N,
        K::KeyM => keymap::KEY_M,
        K::Comma => keymap::KEY_COMMA,
        K::Period => keymap::KEY_DOT,
        K::Slash => keymap::KEY_SLASH,
        K::ShiftRight => keymap::KEY_RIGHTSHIFT,
        K::NumpadMultiply => keymap::KEY_KPASTERISK,
        K::AltLeft => keymap::KEY_LEFTALT,
        K::Space => keymap::KEY_SPACE,
        K::CapsLock => keymap::KEY_CAPSLOCK,
        K::F1 => keymap::KEY_F1,
        K::F2 => keymap::KEY_F2,
        K::F3 => keymap::KEY_F3,
        K::F4 => keymap::KEY_F4,
        K::F5 => keymap::KEY_F5,
        K::F6 => keymap::KEY_F6,
        K::F7 => keymap::KEY_F7,
        K::F8 => keymap::KEY_F8,
        K::F9 => keymap::KEY_F9,
        K::F10 => keymap::KEY_F10,
        K::NumLock => keymap::KEY_NUMLOCK,
        K::ScrollLock => keymap::KEY_SCROLLLOCK,
        K::Numpad7 => keymap::KEY_KP7,
        K::Numpad8 => keymap::KEY_KP8,
        K::Numpad9 => keymap::KEY_KP9,
        K::NumpadSubtract => keymap::KEY_KPMINUS,
        K::Numpad4 => keymap::KEY_KP4,
        K::Numpad5 => keymap::KEY_KP5,
        K::Numpad6 => keymap::KEY_KP6,
        K::NumpadAdd => keymap::KEY_KPPLUS,
        K::Numpad1 => keymap::KEY_KP1,
        K::Numpad2 => keymap::KEY_KP2,
        K::Numpad3 => keymap::KEY_KP3,
        K::Numpad0 => keymap::KEY_KP0,
        K::NumpadDecimal => keymap::KEY_KPDOT,
        K::F11 => keymap::KEY_F11,
        K::F12 => keymap::KEY_F12,
        K::ControlRight => keymap::KEY_RIGHTCTRL,
        K::NumpadDivide => keymap::KEY_KPSLASH,
        K::AltRight => keymap::KEY_RIGHTALT,
        K::Home => keymap::KEY_HOME,
        K::ArrowUp => keymap::KEY_UP,
        K::PageUp => keymap::KEY_PAGEUP,
        K::ArrowLeft => keymap::KEY_LEFT,
        K::ArrowRight => keymap::KEY_RIGHT,
        K::End => keymap::KEY_END,
        K::ArrowDown => keymap::KEY_DOWN,
        K::PageDown => keymap::KEY_PAGEDOWN,
        K::Insert => keymap::KEY_INSERT,
        K::Delete => keymap::KEY_DELETE,
        K::AudioVolumeMute => keymap::KEY_MUTE,
        K::AudioVolumeDown => keymap::KEY_VOLUMEDOWN,
        K::AudioVolumeUp => keymap::KEY_VOLUMEUP,
        K::SuperLeft => keymap::KEY_LEFTMETA,
        K::SuperRight => keymap::KEY_RIGHTMETA,
        K::Pause => keymap::KEY_PAUSE,
        K::PrintScreen => keymap::KEY_PRINT,
        K::F13 => keymap::KEY_F13,
        K::F14 => keymap::KEY_F14,
        K::F15 => keymap::KEY_F15,
        K::F16 => keymap::KEY_F16,
        K::F17 => keymap::KEY_F17,
        K::F18 => keymap::KEY_F18,
        K::F19 => keymap::KEY_F19,
        K::F20 => keymap::KEY_F20,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Control pump (SINGLE-OWNER thread over one shared connection)
// ---------------------------------------------------------------------------

/// Non-blocking bidirectional control-plane bridge.
///
/// Threading rule (live-documented deadlock): Win32 AF_HYPERV sockets must
/// NOT run concurrent blocking `recv()` (infinite timeout) and `send()` from
/// two threads on the same handle — the two-thread pump stalled input
/// forwarding forever. This pump uses ONE owner thread that exclusively
/// drives the connection:
/// 1. drains all pending outbound messages (`try_recv()` loop ? `send_msg`),
/// 2. gates inbound checks with Winsock `select()` (2ms) — a readable socket
///    means `recv_msg` completes without stalling; no inbound data = skip,
/// 3. never blocks the winit UI thread (bounded mpsc + drain).
pub struct ControlPump {
    outbound: std::sync::mpsc::Sender<ControlMessage>,
    inbound: Arc<Mutex<Vec<ControlMessage>>>,
    stop: Arc<AtomicBool>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Set by the pump when the guest confirms a StopSession.
    torn_down: Arc<(Mutex<Option<bool>>, Condvar)>,
}

impl ControlPump {
    /// Takes shared ownership (`Arc`) — the pump thread is the ONLY thread
    /// that drives the socket; other holders keep it alive for emergency
    /// teardown only.
    pub fn start(conn: Arc<HvConnection>) -> Self {
        let (outbound, outbound_rx) = std::sync::mpsc::channel::<ControlMessage>();
        let inbound: Arc<Mutex<Vec<ControlMessage>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let torn_down = Arc::new((Mutex::new(None), Condvar::new()));

        let conn = Arc::new(conn);
        let thread_conn = conn.clone();
        let thread_stop = stop.clone();
        let thread_inbound = inbound.clone();
        let thread_torn = torn_down.clone();
        let thread = std::thread::Builder::new()
            .name("wsldesk-ctrl-pump".into())
            .spawn(move || {
                loop {
                    if thread_stop.load(Ordering::SeqCst) {
                        break;
                    }

                    // 1. Drain ALL pending outbound messages immediately —
                    //    input forwarding must never queue behind reads.
                    loop {
                        match outbound_rx.try_recv() {
                            Ok(msg) => {
                                let result = thread_conn.send_msg(&msg);
                                if DEBUG_INPUT.load(Ordering::Relaxed) {
                                    if let ControlMessage::InputEvent(common::protocol::InputPayload::Key { code, pressed }) = &msg {
                                        eprintln!("[PUMP-TX] key code={code} pressed={pressed} err={:?}", result.as_ref().err());
                                    }
                                }
                                if let Err(err) = result {
                                    eprintln!("control pump send failed: {err}");
                                    return;
                                }
                            }
                            Err(std::sync::mpsc::TryRecvError::Empty) => break,
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                        }
                    }

                    // 2. Inbound check WITHOUT stalling outbound traffic:
                    //    2ms select() gate, then a full recv_msg (data is
                    //    buffered, so recv cannot block).
                    if transport::select_readable(
                        thread_conn.raw_socket(),
                        Duration::from_millis(2),
                    ) {
                        match thread_conn.recv_msg() {
                            Ok(msg) => {
                                if let ControlMessage::SessionStopped { clean_exit, .. } =
                                    &msg
                                {
                                    let (lock, signal) = &*thread_torn;
                                    *lock.lock().expect("torn lock") = Some(*clean_exit);
                                    signal.notify_all();
                                }
                                thread_inbound.lock().expect("inbound lock").push(msg);
                            }
                            Err(err) => {
                                eprintln!("control pump: {err}");
                                break;
                            }
                        }
                    }
                }
            })
            .expect("spawn control pump thread");

        Self {
            outbound,
            inbound,
            stop,
            thread: Mutex::new(Some(thread)),
            torn_down,
        }
    }

    /// Queue an outbound message (never blocks the caller).
    pub fn send(&self, msg: ControlMessage) {
        let _ = self.outbound.send(msg);
    }

    /// Take all inbound messages accumulated since the last drain.
    pub fn drain_inbound(&self) -> Vec<ControlMessage> {
        std::mem::take(&mut self.inbound.lock().expect("inbound lock"))
    }

    /// Request guest teardown and wait for the `SessionStopped` confirmation.
    /// Returns `None` when the guest did not confirm in time.
    pub fn stop_session_and_wait(&self, timeout: Duration) -> Option<bool> {
        self.send(ControlMessage::StopSession {
            session_id: String::new(),
        });
        let (lock, signal) = &*self.torn_down;
        let guard = lock.lock().expect("torn lock");
        let (guard, _res) = signal
            .wait_timeout_while(guard, timeout, |v| v.is_none())
            .expect("torn wait");
        *guard
    }

    /// Shut the pump down (stop flag + join the owner thread).
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.thread.lock().expect("threads lock").take() {
            let _ = handle.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Win32 clipboard bridge (CF_UNICODETEXT, SHA-256 dedup)
// ---------------------------------------------------------------------------

/// `CF_UNICODETEXT` clipboard format id (13; kept as a constant to avoid
/// pulling the whole Ole feature for one format constant).
const CF_UNICODETEXT: u32 = 13;

/// Watch the Windows clipboard for foreign changes (polls the clipboard
/// sequence number â€” cheap and robust) and push host clipboard text to the
/// guest. Dedup: never re-offer the text we last offered ourselves.
pub fn spawn_clipboard_watcher(
    pump: Arc<ControlPump>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("wsldesk-clip".into())
        .spawn(move || {
            let mut last_seq = get_clipboard_sequence();
            let mut last_offered_hash = String::new();
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(300));
                let seq = get_clipboard_sequence();
                if seq == last_seq {
                    continue;
                }
                last_seq = seq;
                if let Some(text) = read_clipboard_text() {
                    let hash = clipboard_hash(&text);
                    if hash == last_offered_hash {
                        continue; // our own teardown echo
                    }
                    last_offered_hash = hash;
                    eprintln!(
                        "host clipboard changed â€” offering to guest ({} chars)",
                        text.chars().count()
                    );
                    pump.send(ControlMessage::ClipboardUpdate {
                        mime_type: "text/plain;charset=utf-8".into(),
                        data: text.into_bytes(),
                    });
                }
            }
        })
        .expect("spawn clipboard watcher")
}

/// Apply guestâ†’host clipboard updates received on the control plane.
/// Returns true when the clipboard was set; dedup happens against the last
/// text the Windows clipboard provided so a paste loop never occurs.
pub fn apply_guest_clipboard(text: &str, last_seen_hash: &mut String) -> bool {
    let hash = clipboard_hash(text);
    if hash == *last_seen_hash {
        return false;
    }
    *last_seen_hash = hash.clone();
    set_clipboard_text(text)
}

pub fn clipboard_hash(text: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(text.as_bytes()))
}

fn get_clipboard_sequence() -> u32 {
    unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() }
}

/// Read `CF_UNICODETEXT` from the Windows clipboard (None when absent).
fn read_clipboard_text() -> Option<String> {
    unsafe {
        use windows::Win32::System::DataExchange::{
            CloseClipboard, GetClipboardData, OpenClipboard,
        };
        use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
        use windows::Win32::Foundation::HGLOBAL;
        if OpenClipboard(None).is_err() {
            return None;
        }
        let result = (|| {
            let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
            let hglobal = HGLOBAL(handle.0);
            let ptr = GlobalLock(hglobal);
            if ptr.is_null() {
                return None;
            }
            // The clipboard stores UTF-16; find the NUL terminator length.
            let wide: Vec<u16> = {
                let mut out = Vec::new();
                let mut p = ptr as *const u16;
                while *p != 0 {
                    out.push(*p);
                    p = p.add(1);
                }
                out
            };
            let _ = GlobalUnlock(hglobal);
            String::from_utf16(&wide).ok()
        })();
        let _ = CloseClipboard();
        result
    }
}

/// Set `CF_UNICODETEXT` on the Windows clipboard (best effort; a NULL owner
/// works for short-lived cross-thread access).
fn set_clipboard_text(text: &str) -> bool {
    unsafe {
        use windows::Win32::System::DataExchange::{
            CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
        };
        use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
        use windows::Win32::Foundation::HGLOBAL;
        if OpenClipboard(None).is_err() {
            return false;
        }
        let ok = (|| -> Option<bool> {
            if EmptyClipboard().is_err() {
                return Some(false);
            }
            let mut wide: Vec<u16> = text.encode_utf16().collect();
            wide.push(0);
            let bytes = wide.len() * 2;
            let hglobal: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, bytes).ok()?;
            let ptr = GlobalLock(hglobal);
            if ptr.is_null() {
                return Some(false);
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr as *mut u16, wide.len());
            let _ = GlobalUnlock(hglobal);
            // SetClipboardData takes ownership of hglobal on success.
            SetClipboardData(CF_UNICODETEXT, windows::Win32::Foundation::HANDLE(hglobal.0)).ok()?;
            Some(true)
        })()
        .unwrap_or(false);
        let _ = CloseClipboard();
        ok
    }
}

// ---------------------------------------------------------------------------
// Raw keyboard grab (WH_KEYBOARD_LL): Super + Alt+Tab pass through to Linux
// ---------------------------------------------------------------------------

/// Toggle request raised by the LL hook (Right Ctrl + F fullscreen).
pub static FULLSCREEN_TOGGLE_REQUESTED: AtomicBool = AtomicBool::new(false);
pub static DEBUG_INPUT: AtomicBool = AtomicBool::new(false);

/// State shared with the hook procedure (installed before the hook).
struct HookState {
    grab: Arc<AtomicBool>,
    hwnd: Arc<AtomicUsize>,
    pump: Arc<ControlPump>,
}

static HOOK_STATE: OnceLock<HookState> = OnceLock::new();

/// Own the hook on the viewport's already-running Win32 message loop.
pub struct KeyboardHook(u32, Option<std::thread::JoinHandle<()>>);

impl Drop for KeyboardHook {
    fn drop(&mut self) {
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_QUIT};
            use windows::Win32::Foundation::{WPARAM, LPARAM};
            if PostThreadMessageW(self.0, WM_QUIT, WPARAM(0), LPARAM(0)).is_ok() {
                if let Some(thread) = self.1.take() { let _ = thread.join(); }
            }
        }
    }
}

/// Install after window/GPU initialization. The hook is active ONLY while
/// `grab_enabled` is true AND `hwnd` is the foreground window; Super and
/// Alt+Tab are forwarded to Linux and swallowed on Windows. Right Ctrl + F
/// raises [`FULLSCREEN_TOGGLE_REQUESTED`].
pub fn install_keyboard_grab(
    grab_enabled: Arc<AtomicBool>,
    hwnd: Arc<AtomicUsize>,
    pump: Arc<ControlPump>,
) -> windows::core::Result<KeyboardHook> {
    let _ = HOOK_STATE.set(HookState { grab: grab_enabled, hwnd, pump });
    let (ready, installed) = std::sync::mpsc::sync_channel(1);
    let thread = std::thread::spawn(move || unsafe {
        use windows::Win32::UI::WindowsAndMessaging::*;
        let mut msg = MSG::default();
        let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
        let hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_proc), None, 0) {
            Ok(hook) => hook,
            Err(err) => { let _ = ready.send(Err(err)); return; }
        };
        let id = windows::Win32::System::Threading::GetCurrentThreadId();
        let _ = ready.send(Ok(id));
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = UnhookWindowsHookEx(hook);
    });
    Ok(KeyboardHook(installed.recv().expect("hook startup")?, Some(thread)))
}

const WM_KEYDOWN: usize = 0x0100;
const WM_KEYUP: usize = 0x0101;
const WM_SYSKEYDOWN: usize = 0x0104;
const WM_SYSKEYUP: usize = 0x0105;
const VK_TAB: i32 = 0x09;
const VK_MENU_ALT: i32 = 0x12;
const VK_LWIN: i32 = 0x5B;
const VK_RWIN: i32 = 0x5C;
const VK_RCONTROL: i32 = 0xA3;
const VK_F: i32 = 0x46;

#[allow(non_snake_case)]
unsafe extern "system" fn ll_proc(
    code: i32,
    wParam: windows::Win32::Foundation::WPARAM,
    lParam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::Foundation::LRESULT;
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetForegroundWindow, KBDLLHOOKSTRUCT,
    };
    if let Some(state) = HOOK_STATE.get() {
        let fg = GetForegroundWindow().0 as usize;
        let target = state.hwnd.load(Ordering::SeqCst);
        let grab = state.grab.load(Ordering::SeqCst);
        if code >= 0 && fg == target && target != 0 {
            let info = &*(lParam.0 as *const KBDLLHOOKSTRUCT);
            let vk = info.vkCode as i32;
            let pressed = match wParam.0 {
                WM_KEYDOWN | WM_SYSKEYDOWN => true,
                WM_KEYUP | WM_SYSKEYUP => false,
                _ => return CallNextHookEx(None, code, wParam, lParam),
            };
            let alt_tab = vk == VK_TAB && GetAsyncKeyState(VK_MENU_ALT) as u16 & 0x8000 != 0;
            let fullscreen = vk == VK_F && GetAsyncKeyState(VK_RCONTROL) as u16 & 0x8000 != 0 && pressed;
            let forward = grab && (matches!(vk, VK_LWIN | VK_RWIN | 13) || alt_tab);
            if DEBUG_INPUT.load(Ordering::Relaxed) {
                let action = if forward { "forward+swallow" } else if grab && fullscreen { "fullscreen" } else { "pass" };
                eprintln!("[HOOK] vk=0x{vk:X} msg=0x{:X} fg={fg:#x} target={target:#x} grab={grab} action={action}", wParam.0);
            }
            if !grab { return CallNextHookEx(None, code, wParam, lParam); }
            if vk == 13 {
                let code = if info.flags.0 & 1 != 0 { 96 } else { keymap::KEY_ENTER };
                state.pump.send(ControlMessage::InputEvent(common::protocol::InputPayload::Key { code, pressed }));
                return LRESULT(1);
            }
            // Super keys: forward + swallow.
            if vk == VK_LWIN || vk == VK_RWIN {
                let evdev = if vk == VK_LWIN {
                    keymap::KEY_LEFTMETA
                } else {
                    keymap::KEY_RIGHTMETA
                };
                state.pump.send(ControlMessage::InputEvent(
                    common::protocol::InputPayload::Key { code: evdev, pressed },
                ));
                return LRESULT(1);
            }
            // Alt+Tab: forward Tab to Linux, swallow on Windows.
            if alt_tab {
                state.pump.send(ControlMessage::InputEvent(
                    common::protocol::InputPayload::Key {
                        code: keymap::KEY_TAB,
                        pressed,
                    },
                ));
                return LRESULT(1);
            }
            // Right Ctrl + F: fullscreen toggle (host-side action, swallowed).
            if fullscreen {
                FULLSCREEN_TOGGLE_REQUESTED.store(true, Ordering::SeqCst);
                return LRESULT(1);
            }
        }
    }
    CallNextHookEx(None, code, wParam, lParam)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn winit_letters_map_to_evdev_positions() {
        assert_eq!(winit_key_to_evdev(winit::keyboard::KeyCode::KeyQ), Some(keymap::KEY_Q));
        assert_eq!(winit_key_to_evdev(winit::keyboard::KeyCode::KeyA), Some(keymap::KEY_A));
        assert_eq!(winit_key_to_evdev(winit::keyboard::KeyCode::KeyZ), Some(keymap::KEY_Z));
        assert_eq!(winit_key_to_evdev(winit::keyboard::KeyCode::KeyM), Some(keymap::KEY_M));
    }

    #[test]
    fn winit_specials_map() {
        use winit::keyboard::KeyCode as K;
        assert_eq!(winit_key_to_evdev(K::Escape), Some(1));
        assert_eq!(winit_key_to_evdev(K::Enter), Some(28));
        assert_eq!(winit_key_to_evdev(K::Space), Some(57));
        assert_eq!(winit_key_to_evdev(K::ArrowUp), Some(103));
        assert_eq!(winit_key_to_evdev(K::Delete), Some(111));
        assert_eq!(winit_key_to_evdev(K::SuperLeft), Some(125));
        assert_eq!(winit_key_to_evdev(K::SuperRight), Some(126));
        assert_eq!(winit_key_to_evdev(K::F12), Some(88));
        assert_eq!(winit_key_to_evdev(K::PrintScreen), Some(210));
        // NumpadEnter has no mapping entry â€” exercises the None arm.
        assert_eq!(winit_key_to_evdev(K::NumpadEnter), None);
    }

    #[test]
    fn clipboard_hash_is_stable_and_differs() {
        let a = clipboard_hash("hello");
        assert_eq!(a, clipboard_hash("hello"));
        assert_ne!(a, clipboard_hash("world"));
        assert_eq!(a.len(), 64);
    }
}
