//! Linux evdev keycode definitions (Plan.Md §4.1/§5 M4B).
//!
//! These are the codes carried on the wire in
//! `ControlMessage::InputEvent(InputPayload::Key { code, .. })` — Linux
//! evdev keycodes as defined by `linux/input-event-codes.h`, the canonical
//! Wayland input code space. The Windows host maps `winit::keyboard::KeyCode`
//! to these before sending; the guest forwards them to
//! `zwp_virtual_keyboard_v1::key` (which expects evdev+8 — the +8 happens on
//! the guest side).
//!
//! Mouse buttons are the Linux `BTN_*` range (0x110–0x114).
//!
//! This module is intentionally data-only (no unsafe, no platform types) so
//! `common` stays OS-agnostic; the winit-side mapping table lives in the
//! host crate where `winit` is a real dependency.

#![allow(dead_code)] // full table: not every code is referenced by the host map yet

// ---------------------------------------------------------------------------
// Keyboard (evdev keycodes)
// ---------------------------------------------------------------------------

pub const KEY_RESERVED: u32 = 0;
pub const KEY_ESC: u32 = 1;
pub const KEY_1: u32 = 2;
pub const KEY_2: u32 = 3;
pub const KEY_3: u32 = 4;
pub const KEY_4: u32 = 5;
pub const KEY_5: u32 = 6;
pub const KEY_6: u32 = 7;
pub const KEY_7: u32 = 8;
pub const KEY_8: u32 = 9;
pub const KEY_9: u32 = 10;
pub const KEY_0: u32 = 11;
pub const KEY_MINUS: u32 = 12;
pub const KEY_EQUAL: u32 = 13;
pub const KEY_BACKSPACE: u32 = 14;
pub const KEY_TAB: u32 = 15;
pub const KEY_Q: u32 = 16;
pub const KEY_W: u32 = 17;
pub const KEY_E: u32 = 18;
pub const KEY_R: u32 = 19;
pub const KEY_T: u32 = 20;
pub const KEY_Y: u32 = 21;
pub const KEY_U: u32 = 22;
pub const KEY_I: u32 = 23;
pub const KEY_O: u32 = 24;
pub const KEY_P: u32 = 25;
pub const KEY_LEFTBRACE: u32 = 26;
pub const KEY_RIGHTBRACE: u32 = 27;
pub const KEY_ENTER: u32 = 28;
pub const KEY_LEFTCTRL: u32 = 29;
pub const KEY_A: u32 = 30;
pub const KEY_S: u32 = 31;
pub const KEY_D: u32 = 32;
pub const KEY_F: u32 = 33;
pub const KEY_G: u32 = 34;
pub const KEY_H: u32 = 35;
pub const KEY_J: u32 = 36;
pub const KEY_K: u32 = 37;
pub const KEY_L: u32 = 38;
pub const KEY_SEMICOLON: u32 = 39;
pub const KEY_APOSTROPHE: u32 = 40;
pub const KEY_GRAVE: u32 = 41;
pub const KEY_LEFTSHIFT: u32 = 42;
pub const KEY_BACKSLASH: u32 = 43;
pub const KEY_Z: u32 = 44;
pub const KEY_X: u32 = 45;
pub const KEY_C: u32 = 46;
pub const KEY_V: u32 = 47;
pub const KEY_B: u32 = 48;
pub const KEY_N: u32 = 49;
pub const KEY_M: u32 = 50;
pub const KEY_COMMA: u32 = 51;
pub const KEY_DOT: u32 = 52;
pub const KEY_SLASH: u32 = 53;
pub const KEY_RIGHTSHIFT: u32 = 54;
pub const KEY_KPASTERISK: u32 = 55;
pub const KEY_LEFTALT: u32 = 56;
pub const KEY_SPACE: u32 = 57;
pub const KEY_CAPSLOCK: u32 = 58;
pub const KEY_F1: u32 = 59;
pub const KEY_F2: u32 = 60;
pub const KEY_F3: u32 = 61;
pub const KEY_F4: u32 = 62;
pub const KEY_F5: u32 = 63;
pub const KEY_F6: u32 = 64;
pub const KEY_F7: u32 = 65;
pub const KEY_F8: u32 = 66;
pub const KEY_F9: u32 = 67;
pub const KEY_F10: u32 = 68;
pub const KEY_NUMLOCK: u32 = 69;
pub const KEY_SCROLLLOCK: u32 = 70;
pub const KEY_KP7: u32 = 71;
pub const KEY_KP8: u32 = 72;
pub const KEY_KP9: u32 = 73;
pub const KEY_KPMINUS: u32 = 74;
pub const KEY_KP4: u32 = 75;
pub const KEY_KP5: u32 = 76;
pub const KEY_KP6: u32 = 77;
pub const KEY_KPPLUS: u32 = 78;
pub const KEY_KP1: u32 = 79;
pub const KEY_KP2: u32 = 80;
pub const KEY_KP3: u32 = 81;
pub const KEY_KP0: u32 = 82;
pub const KEY_KPDOT: u32 = 83;
pub const KEY_F11: u32 = 87;
pub const KEY_F12: u32 = 88;
pub const KEY_RIGHTCTRL: u32 = 97;
pub const KEY_KPSLASH: u32 = 98;
pub const KEY_RIGHTALT: u32 = 100;
pub const KEY_HOME: u32 = 102;
pub const KEY_UP: u32 = 103;
pub const KEY_PAGEUP: u32 = 104;
pub const KEY_LEFT: u32 = 105;
pub const KEY_RIGHT: u32 = 106;
pub const KEY_END: u32 = 107;
pub const KEY_DOWN: u32 = 108;
pub const KEY_PAGEDOWN: u32 = 109;
pub const KEY_INSERT: u32 = 110;
pub const KEY_DELETE: u32 = 111;
pub const KEY_MUTE: u32 = 113;
pub const KEY_VOLUMEDOWN: u32 = 114;
pub const KEY_VOLUMEUP: u32 = 115;
pub const KEY_PAUSE: u32 = 119;
pub const KEY_LEFTMETA: u32 = 125;
pub const KEY_RIGHTMETA: u32 = 126;
pub const KEY_COMPOSE: u32 = 127;
pub const KEY_F13: u32 = 183;
pub const KEY_F14: u32 = 184;
pub const KEY_F15: u32 = 185;
pub const KEY_F16: u32 = 186;
pub const KEY_F17: u32 = 187;
pub const KEY_F18: u32 = 188;
pub const KEY_F19: u32 = 189;
pub const KEY_F20: u32 = 190;
/// PrintScreen (SYSRQ).
pub const KEY_PRINT: u32 = 210;

// ---------------------------------------------------------------------------
// Mouse buttons (Linux BTN_* range)
// ---------------------------------------------------------------------------

pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;
pub const BTN_SIDE: u16 = 0x113;
pub const BTN_EXTRA: u16 = 0x114;

// ---------------------------------------------------------------------------
// XKB modifier bitmask (depressed_modifier values for
// zwp_virtual_keyboard_v1::modifiers) — the standard XKB modifier indices:
// Shift=0, Lock=1, Control=2, Mod1(Alt)=3, Mod2(NumLock)=4,
// Mod3=5, Mod4(Super/Logo)=6, Mod5=7.
// ---------------------------------------------------------------------------

pub const MOD_SHIFT: u32 = 1 << 0;
pub const MOD_CAPSLOCK: u32 = 1 << 1;
pub const MOD_CONTROL: u32 = 1 << 2;
pub const MOD_ALT: u32 = 1 << 3;
pub const MOD_NUMLOCK: u32 = 1 << 4;
pub const MOD_SUPER: u32 = 1 << 6;

/// Wayland keycodes passed to `zwp_virtual_keyboard_v1::key` are evdev + 8.
pub const WAYLAND_KEY_OFFSET: u32 = 8;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_codes_match_input_event_codes_h() {
        // Spot-check the anchors agents most often get wrong.
        assert_eq!(KEY_ESC, 1);
        assert_eq!(KEY_1, 2);
        assert_eq!(KEY_0, 11);
        assert_eq!(KEY_TAB, 15);
        assert_eq!(KEY_Q, 16);
        assert_eq!(KEY_P, 25);
        assert_eq!(KEY_ENTER, 28);
        assert_eq!(KEY_A, 30);
        assert_eq!(KEY_Z, 44);
        assert_eq!(KEY_M, 50);
        assert_eq!(KEY_SPACE, 57);
        assert_eq!(KEY_CAPSLOCK, 58);
        assert_eq!(KEY_F1, 59);
        assert_eq!(KEY_F12, 88);
        assert_eq!(KEY_RIGHTCTRL, 97);
        assert_eq!(KEY_HOME, 102);
        assert_eq!(KEY_UP, 103);
        assert_eq!(KEY_PAGEUP, 104);
        assert_eq!(KEY_DELETE, 111);
        assert_eq!(KEY_LEFTMETA, 125);
        assert_eq!(KEY_RIGHTMETA, 126);
        assert_eq!(KEY_PRINT, 210);
    }

    #[test]
    fn mouse_buttons_match_linux_btn_range() {
        assert_eq!(BTN_LEFT, 0x110);
        assert_eq!(BTN_RIGHT, 0x111);
        assert_eq!(BTN_MIDDLE, 0x112);
        assert_eq!(BTN_SIDE, 0x113);
        assert_eq!(BTN_EXTRA, 0x114);
    }

    #[test]
    fn xkb_modifier_bits() {
        assert_eq!(MOD_SHIFT, 0b0000_0001);
        assert_eq!(MOD_CAPSLOCK, 0b0000_0010);
        assert_eq!(MOD_CONTROL, 0b0000_0100);
        assert_eq!(MOD_ALT, 0b0000_1000);
        assert_eq!(MOD_SUPER, 0b0100_0000);
    }

    #[test]
    fn wayland_key_offset_is_8() {
        assert_eq!(WAYLAND_KEY_OFFSET, 8);
        assert_eq!(KEY_A + WAYLAND_KEY_OFFSET, 38);
    }
}
