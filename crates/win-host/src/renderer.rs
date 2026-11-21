//! Native Win32 viewport: `winit` + `wgpu` (DX12) (Plan.Md §5 M4A).
//!
//! Data path: a dedicated blocking receiver thread pulls `b"WFRM"` frames
//! from the guest over `VIDEO_PORT`, LZ4-decodes them, and publishes the
//! LATEST frame into a slot (stale frames are overwritten — never queued —
//! so backpressure can never accumulate multi-second video lag). The winit
//! render loop uploads the latest frame via `queue.write_texture()` and
//! draws it fullscreen with a minimal WGSL shader.
//!
//! Channel-order guarantee: the texture format is chosen to match the wire
//! `PixelFormat` byte order (Bgra8Unorm ↔ Bgra8888, Rgba8Unorm ↔ Rgba8888)
//! and the surface format matches the texture — sampled and written
//! unconverted, so reds and blues are never swapped.
//!
//! Teardown: window close or Ctrl+C sends `StopSession` over the control
//! channel before exiting, so no headless compositor or runtime dir leaks.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use common::protocol::{ControlMessage, PixelFormat};
use crate::input::{self, ControlPump};
use crate::transport::{DecodedFrame, HvConnection, TransportError, VideoStream};

/// Latest-frame slot shared between the receiver thread and the render loop.
///
/// Writers always overwrite; readers take-and-clear. Dropping frames under
/// backpressure is deliberate (no multi-second lag).
pub struct FramePublisher {
    latest: Mutex<Option<DecodedFrame>>,
    wake: OnceLock<winit::event_loop::EventLoopProxy<()>>,
    closed: AtomicBool,
}

impl FramePublisher {
    pub fn new() -> Self {
        Self {
            latest: Mutex::new(None),
            wake: OnceLock::new(),
            closed: AtomicBool::new(false),
        }
    }

    /// Overwrite the latest frame and wake the render thread.
    pub fn publish(&self, frame: DecodedFrame) {
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        let was_empty = self.latest.lock().expect("frame lock").replace(frame).is_none();
        // Winit sleeps independently of this mutex. A condition variable
        // cannot wake its Win32 message loop. Coalesce frames until consumed.
        if was_empty {
            self.wake_event_loop();
        }
    }

    /// Non-blocking take of the newest frame (if any).
    pub fn try_take(&self) -> Option<DecodedFrame> {
        self.latest.lock().expect("frame lock").take()
    }

    /// True once the receiver wants the render loop to wind down.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.wake_event_loop();
    }

    fn wake_event_loop(&self) {
        if let Some(proxy) = self.wake.get() {
            let _ = proxy.send_event(());
        }
    }

    /// Newest frame without consuming it (for the redraw pump).
    pub fn peek(&self) -> bool {
        self.latest.lock().expect("frame lock").is_some()
    }
}

/// Live session stats printed by the receiver thread when it finishes.
#[derive(Debug, Default, Clone)]
pub struct VideoStats {
    pub frames: u64,
    pub seconds: f64,
    pub wire_bytes: u64,
    pub raw_bytes: u64,
    pub width: u32,
    pub height: u32,
    pub pixel_format: Option<PixelFormat>,
}

/// Spawn the blocking receiver thread: connect (with retry while the guest
/// binds 50002), then decode frames into `publisher` until stop/closed.
/// Returns the thread handle and a slot that receives final stats.
pub fn spawn_video_receiver(
    distro: &'static str,
    publisher: Arc<FramePublisher>,
    stop: Arc<AtomicBool>,
    stats: Arc<Mutex<VideoStats>>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("wsldesk-video-rx".into())
        .spawn(move || {
            let started = Instant::now();
            let stream = match connect_video_with_retry(distro, &stop) {
                Ok(s) => s,
                Err(err) => {
                    eprintln!("video receiver: {err}");
                    publisher.close();
                    return;
                }
            };
            eprintln!("video receiver: connected to AF_VSOCK {VIDEO_PORT}");
            let mut local = VideoStats::default();
            while !stop.load(Ordering::SeqCst) && !publisher.is_closed() {
                match stream.recv_frame() {
                    Ok(frame) => {
                        local.frames += 1;
                        local.raw_bytes += frame.pixels.len() as u64;
                        local.wire_bytes += frame.wire_len() as u64;
                        local.width = frame.header.width;
                        local.height = frame.header.height;
                        local.pixel_format = Some(frame.header.pixel_format);
                        publisher.publish(frame);
                    }
                    Err(TransportError::Io(err))
                        if err.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        continue; // idle window — keep waiting while not stopped
                    }
                    Err(TransportError::Io(err))
                        if err.kind() == std::io::ErrorKind::UnexpectedEof =>
                    {
                        eprintln!("video receiver: guest closed the video stream");
                        break;
                    }
                    Err(err) => {
                        if !stop.load(Ordering::SeqCst) {
                            eprintln!("video receiver: {err}");
                        }
                        break;
                    }
                }
            }
            local.seconds = started.elapsed().as_secs_f64();
            if let Ok(mut slot) = stats.lock() {
                *slot = local.clone();
            }
            if local.frames > 0 {
                eprintln!(
                    "video receiver stats: {} frames, {:.1} fps, wire {:.2} MB, raw {:.2} MB, compression {:.1}x",
                    local.frames,
                    local.frames as f64 / local.seconds.max(1e-6),
                    local.wire_bytes as f64 / 1_048_576.0,
                    local.raw_bytes as f64 / 1_048_576.0,
                    local.raw_bytes as f64 / local.wire_bytes.max(1) as f64
                );
            }
            publisher.close();
        })
        .expect("spawn video receiver thread")
}

const VIDEO_PORT: &str = "50002";

/// Connect to the video plane with a bounded retry window (the guest binds
/// 50002 right after SessionStarted, so first attempts may race).
fn connect_video_with_retry(
    distro: &str,
    stop: &AtomicBool,
) -> Result<VideoStream, TransportError> {
    let mut last_err: Option<TransportError> = None;
    for _ in 0..20 {
        if stop.load(Ordering::SeqCst) {
            return Err(TransportError::Wsa(0, "video receiver stopped".into()));
        }
        match VideoStream::connect(distro) {
            Ok(stream) => return Ok(stream),
            Err(err @ TransportError::DaemonNotRunning { .. }) => {
                last_err = Some(err);
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(other) => return Err(other),
        }
    }
    Err(last_err.unwrap_or(TransportError::Wsa(0, "video connect failed".into())))
}

// ---------------------------------------------------------------------------
// Teardown plumbing (window close + Ctrl+C — orphan prevention)
// ---------------------------------------------------------------------------

/// The live control connection, reachable from the console handler thread
/// (the pump keeps it alive; the handler sends on the same socket, which is
/// safe for concurrent send/recv on Win32 sockets).
static CONTROL_FOR_SIGNAL: OnceLock<Arc<HvConnection>> = OnceLock::new();

/// Hand the control connection to the static slot used by the Ctrl handler.
pub fn register_ctrl_handler(control: Arc<HvConnection>) {
    let _ = CONTROL_FOR_SIGNAL.set(control);
    unsafe {
        use windows::Win32::System::Console::SetConsoleCtrlHandler;
        let _ = SetConsoleCtrlHandler(Some(ctrl_handler), true);
    }
}

/// Send `StopSession` (empty id = stop the active session) best-effort.
pub fn send_stop_session(conn: &HvConnection) -> Result<(), TransportError> {
    conn.send_msg(&common::protocol::ControlMessage::StopSession {
        session_id: String::new(),
    })
}

fn ctrl_cleanup() {
    eprintln!("\nconsole signal — sending StopSession before exit…");
    if let Some(conn) = CONTROL_FOR_SIGNAL.get() {
        let _ = send_stop_session(conn);
        let _ = conn.recv_msg(); // best effort SessionStopped
    }
}

unsafe extern "system" fn ctrl_handler(ctrl_type: u32) -> windows::Win32::Foundation::BOOL {
    // 0 = CTRL_C_EVENT, 2 = CTRL_CLOSE_EVENT.
    if ctrl_type == 0 || ctrl_type == 2 {
        ctrl_cleanup();
    }
    // FALSE → default termination proceeds after our cleanup.
    windows::Win32::Foundation::BOOL(0)
}

// ---------------------------------------------------------------------------
// The winit + wgpu DX12 viewport
// ---------------------------------------------------------------------------

/// GPU/viewport state created on `resumed`.
struct ViewportState {
    window: Arc<winit::window::Window>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    texture: Option<(wgpu::Texture, wgpu::BindGroup, u32, u32, wgpu::TextureFormat)>,
    title_prefix: String,
    fps_window_start: Instant,
    fps_frames: u64,
}

/// The winit application driving the DX12 viewport with full interactivity.
struct ViewportApp {
    publisher: Arc<FramePublisher>,
    pump: Arc<ControlPump>,
    stop_flag: Arc<AtomicBool>,
    grab_enabled: Arc<AtomicBool>,
    hwnd: Arc<AtomicUsize>,
    keyboard_hook: Option<input::KeyboardHook>,
    scroll_remainder: (f32, f32),
    title_prefix: String,
    /// Live captured-output size (published by the video receiver).
    output_size: Arc<Mutex<(u32, u32)>>,
    state: Option<ViewportState>,
    /// Pressed evdev codes — dedupes OS autorepeat.
    pressed_keys: std::collections::HashSet<u32>,
    right_ctrl_held: bool,
    grab_chord_held: bool,
    /// Right Ctrl + F was handled (fullscreen) — the release then skips the
    /// grab toggle.
    rctrl_hold_consumed: bool,
    pending_resize: Option<(u32, u32, u32)>,
    resize_deadline: Option<Instant>,
    fullscreen: bool,
    last_title: String,
    clipboard_hash: String,
}

impl winit::application::ApplicationHandler for ViewportApp {
    fn user_event(&mut self, event_loop: &winit::event_loop::ActiveEventLoop, _: ()) {
        if self.publisher.is_closed() {
            self.teardown();
            event_loop.exit();
        } else if let Some(state) = &self.state {
            state.window.request_redraw();
        }
    }

    fn resumed(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        match create_viewport(event_loop, &self.title_prefix) {
            Ok(state) => {
                // Start the keyboard grab once we know our window handle.
                let hwnd = window_hwnd_usable(&state.window);
                self.hwnd.store(hwnd, Ordering::SeqCst);
                match input::install_keyboard_grab(
                    self.grab_enabled.clone(), self.hwnd.clone(), self.pump.clone(),
                ) {
                    Ok(hook) => self.keyboard_hook = Some(hook),
                    Err(err) => eprintln!("keyboard grab installation failed: {err}"),
                }
                if self.fullscreen {
                    self.grab_enabled.store(true, Ordering::SeqCst);
                    state
                        .window
                        .set_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
                }
                self.state = Some(state);
                self.refresh_title();
            }
            Err(err) => {
                eprintln!("viewport init failed: {err:#}");
                self.teardown();
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &winit::event_loop::ActiveEventLoop,
        _window_id: winit::window::WindowId,
        event: winit::event::WindowEvent,
    ) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        match event {
            winit::event::WindowEvent::CloseRequested => {
                self.teardown();
                event_loop.exit();
            }
            winit::event::WindowEvent::RedrawRequested => {
                if let Err(err) = render_frame(state, &self.publisher) {
                    eprintln!("render error: {err:#}");
                    self.teardown();
                    event_loop.exit();
                    return;
                }
                if let Some((_, _, width, height, _)) = &state.texture {
                    *self.output_size.lock().expect("output size lock") = (*width, *height);
                }
            }
            winit::event::WindowEvent::Resized(size) => {
                if size.width > 0 && size.height > 0 {
                    // Debounce: reconfigure the swapchain immediately, but
                    // only forward to the guest after ~150ms of quiet, with
                    // even, sane dimensions and the real monitor scale.
                    let mut cfg = state.surface_config.clone();
                    cfg.width = size.width;
                    cfg.height = size.height;
                    state.surface_config = cfg.clone();
                    state.surface.configure(&state.device, &cfg);
                    let scale_milli = (state.window.scale_factor() * 1000.0).round() as u32;
                    self.pending_resize = Some((
                        clamp_resize_dim(size.width),
                        clamp_resize_dim(size.height),
                        scale_milli,
                    ));
                    self.resize_deadline = Some(Instant::now() + Duration::from_millis(150));
                }
                state.window.request_redraw();
            }
            winit::event::WindowEvent::KeyboardInput {
                event:
                    winit::event::KeyEvent {
                        physical_key,
                        state: key_state,
                        ..
                    },
                ..
            } => {
                let evdev = match physical_key {
                    winit::keyboard::PhysicalKey::Code(code) => input::winit_key_to_evdev(code),
                    _ => None,
                };
                let focused = state.window.has_focus();
                let grab = self.grab_enabled.load(Ordering::SeqCst);
                if input::DEBUG_INPUT.load(Ordering::Relaxed) {
                    eprintln!("[WINIT] phys={physical_key:?} state={key_state:?} evdev={evdev:?} focused={focused} grab={grab}");
                }
                if !focused { return; }
                let Some(evdev) = evdev else { return; };
                let pressed = key_state == winit::event::ElementState::Pressed;
                // Dedupe OS autorepeat (guest compositors auto-repeat too).
                if pressed {
                    if !self.pressed_keys.insert(evdev) {
                        return;
                    }
                } else if !self.pressed_keys.remove(&evdev) {
                    // Some Windows shortcut handlers consume the Enter press
                    // but leave its release and both Super edges for winit.
                    if grab && evdev == common::keymap::KEY_ENTER &&
                        [common::keymap::KEY_LEFTMETA, common::keymap::KEY_RIGHTMETA]
                            .iter().any(|k| self.pressed_keys.contains(k)) {
                        self.pump.send(ControlMessage::InputEvent(common::protocol::InputPayload::Key { code: evdev, pressed: true }));
                    } else { return; }
                }
                use common::keymap as k;
                if evdev == k::KEY_G {
                    if !pressed && self.grab_chord_held {
                        self.grab_chord_held = false;
                        return;
                    }
                    if pressed && [k::KEY_LEFTCTRL, k::KEY_RIGHTCTRL].iter().any(|v| self.pressed_keys.contains(v))
                        && [k::KEY_LEFTALT, k::KEY_RIGHTALT].iter().any(|v| self.pressed_keys.contains(v)) {
                        self.grab_chord_held = true;
                        self.rctrl_hold_consumed = self.right_ctrl_held;
                        self.grab_enabled.store(!grab, Ordering::SeqCst);
                        self.refresh_title();
                        return;
                    }
                }
                // Right Ctrl semantics (item 3 of the input fix):
                // - plain Right Ctrl press toggles the grab;
                // - Right Ctrl + F toggles fullscreen WITHOUT flipping the
                //   grab state on the same press (the F press marks the hold
                //   as consumed; the Right Ctrl release then skips toggle).
                if evdev == common::keymap::KEY_RIGHTCTRL {
                    if pressed {
                        self.rctrl_hold_consumed = false;
                    } else if !self.rctrl_hold_consumed {
                        let new_state = !self.grab_enabled.load(Ordering::SeqCst);
                        self.grab_enabled.store(new_state, Ordering::SeqCst);
                        tracing::info!(grabbed = new_state, "keyboard grab toggled");
                        self.refresh_title();
                    }
                    self.right_ctrl_held = pressed;
                    self.pump.send(ControlMessage::InputEvent(
                        common::protocol::InputPayload::Key {
                            code: evdev,
                            pressed,
                        },
                    ));
                    return;
                }
                if evdev == common::keymap::KEY_F && pressed && self.right_ctrl_held {
                    // Fullscreen toggle while Right Ctrl is held (the LL hook
                    // swallows this while grabbed; this path handles the
                    // non-grabbed mode). The RCtrl hold is "consumed" so its
                    // release does not also toggle the grab.
                    self.rctrl_hold_consumed = true;
                    self.toggle_fullscreen();
                    return;
                }
                self.pump.send(ControlMessage::InputEvent(
                    common::protocol::InputPayload::Key {
                        code: evdev,
                        pressed,
                    },
                ));
            }
            winit::event::WindowEvent::CursorMoved { position, .. } => {
                let (out_w, out_h) = *self.output_size.lock().expect("output size lock");
                if out_w == 0 || out_h == 0 {
                    return;
                }
                let inner = state.window.inner_size();
                if inner.width == 0 || inner.height == 0 {
                    return;
                }
                let x = ((position.x / inner.width as f64) * out_w as f64)
                    .clamp(0.0, (out_w - 1) as f64) as u32;
                let y = ((position.y / inner.height as f64) * out_h as f64)
                    .clamp(0.0, (out_h - 1) as f64) as u32;
                // Single-cursor UX: the Wayland cursor is rendered into the
                // stream (overlay_cursor = 1), so hide the Windows cursor
                // while it is over the focused viewport.
                state.window.set_cursor_visible(false);
                self.pump.send(ControlMessage::InputEvent(
                    common::protocol::InputPayload::PointerMotionAbsolute {
                        x,
                        y,
                        width: out_w,
                        height: out_h,
                    },
                ));
            }
            winit::event::WindowEvent::CursorLeft { .. } => {
                state.window.set_cursor_visible(true);
            }
            winit::event::WindowEvent::MouseInput { state: btn_state, button, .. } => {
                // A deliberate click captures keyboard shortcuts, including
                // Super+Enter, on every fresh launch and after focus loss.
                if btn_state == winit::event::ElementState::Pressed
                    && button == winit::event::MouseButton::Left
                    && !self.grab_enabled.swap(true, Ordering::SeqCst)
                {
                    self.refresh_title();
                }
                let button = match button {
                    winit::event::MouseButton::Left => common::keymap::BTN_LEFT,
                    winit::event::MouseButton::Right => common::keymap::BTN_RIGHT,
                    winit::event::MouseButton::Middle => common::keymap::BTN_MIDDLE,
                    winit::event::MouseButton::Back => common::keymap::BTN_SIDE,
                    winit::event::MouseButton::Forward => common::keymap::BTN_EXTRA,
                    _ => return,
                };
                let pressed = btn_state == winit::event::ElementState::Pressed;
                self.pump.send(ControlMessage::InputEvent(
                    common::protocol::InputPayload::PointerButton { button, pressed },
                ));
            }
            winit::event::WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    winit::event::MouseScrollDelta::LineDelta(x, y) => (x, y),
                    winit::event::MouseScrollDelta::PixelDelta(pos) => {
                        ((pos.x / 40.0) as f32, (pos.y / 40.0) as f32)
                    }
                };
                // winit: positive y = scroll up; wayland vertical positive =
                // down → flip. Horizontal matches directly.
                self.scroll_remainder.0 += dx;
                self.scroll_remainder.1 -= dy;
                let (x, y) = (self.scroll_remainder.0.trunc(), self.scroll_remainder.1.trunc());
                self.scroll_remainder.0 -= x;
                self.scroll_remainder.1 -= y;
                if y != 0.0 {
                    self.pump.send(ControlMessage::InputEvent(
                        common::protocol::InputPayload::PointerAxis {
                            vertical: true,
                            delta: y as i32,
                        },
                    ));
                }
                if x != 0.0 {
                    self.pump.send(ControlMessage::InputEvent(
                        common::protocol::InputPayload::PointerAxis {
                            vertical: false,
                            delta: x as i32,
                        },
                    ));
                }
            }
            winit::event::WindowEvent::Focused(focused) => {
                // Focus loss: restore the Windows cursor and release the
                // grab (auto-release requirement).
                if !focused {
                    self.scroll_remainder = (0.0, 0.0);
                    state.window.set_cursor_visible(true);
                    // A key held across focus loss may never produce a
                    // release here. Do not leave the guest's modifiers stuck
                    // or deduplicate the next press after focus returns.
                    for code in self.pressed_keys.drain().chain([
                        common::keymap::KEY_LEFTMETA,
                        common::keymap::KEY_RIGHTMETA,
                    ]) {
                        self.pump.send(ControlMessage::InputEvent(
                            common::protocol::InputPayload::Key { code, pressed: false },
                        ));
                    }
                    self.right_ctrl_held = false;
                    self.grab_chord_held = false;
                    self.rctrl_hold_consumed = false;
                    if self.grab_enabled.load(Ordering::SeqCst) {
                        self.grab_enabled.store(false, Ordering::SeqCst);
                        self.refresh_title();
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &winit::event_loop::ActiveEventLoop) {
        // 1. Control-plane inbound: guest clipboard → Windows clipboard.
        for msg in self.pump.drain_inbound() {
            match msg {
                ControlMessage::ClipboardUpdate { mime_type, data } => {
                    if mime_type == "text/plain;charset=utf-8" {
                        if let Ok(text) = String::from_utf8(data) {
                            let text = text.trim_end_matches('\u{0}').to_string();
                            eprintln!("guest clipboard received ({} chars) — setting Windows clipboard", text.chars().count());
                            input::apply_guest_clipboard(&text, &mut self.clipboard_hash);
                        }
                    }
                }
                ControlMessage::Error { code, message } => {
                    eprintln!("guest error ({code}): {message}");
                }
                _ => {}
            }
        }
        // 2. Hook-requested fullscreen toggle (Right Ctrl + F while grabbed).
        //    The Right Ctrl keydown passed through winit first — mark the
        //    hold as consumed so its release does NOT also toggle the grab.
        if input::FULLSCREEN_TOGGLE_REQUESTED.swap(false, Ordering::SeqCst) {
            if self.right_ctrl_held {
                self.rctrl_hold_consumed = true;
            }
            self.toggle_fullscreen();
        }
        // 3. Debounced resize forwarding.
        if let (Some((w, h, scale)), Some(deadline)) =
            (self.pending_resize, self.resize_deadline)
        {
            if Instant::now() >= deadline {
                self.pump.send(ControlMessage::Resize {
                    width: w,
                    height: h,
                    scale_milli: scale,
                });
                self.pending_resize = None;
                self.resize_deadline = None;
            }
        }
        // 4. Redraw when a fresh frame landed (sender caps at ~60 FPS).
        if self.publisher.peek() {
            if let Some(state) = self.state.as_ref() {
                state.window.request_redraw();
            }
        }
    }
}

impl ViewportApp {
    /// Send StopSession, wait for the confirmation, and shut the pump down.
    fn teardown(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(clean) = self.pump.stop_session_and_wait(Duration::from_secs(5)) {
            println!(
                "session teardown: {}",
                if clean { "CLEAN" } else { "UNCLEAN" }
            );
        } else {
            eprintln!("session teardown: no confirmation from guest (may still be stopping)");
        }
        self.pump.shutdown();
    }

    fn toggle_fullscreen(&mut self) {
        let Some(state) = self.state.as_ref() else {
            return;
        };
        self.fullscreen = !self.fullscreen;
        let target = if self.fullscreen {
            Some(winit::window::Fullscreen::Borderless(None))
        } else {
            None
        };
        state.window.set_fullscreen(target);
        if self.fullscreen {
            self.grab_enabled.store(true, Ordering::SeqCst);
            self.refresh_title();
        }
    }

    fn refresh_title(&mut self) {
        let grabbed = self.grab_enabled.load(Ordering::SeqCst);
        let suffix = if grabbed {
            "[GRABBED — Right Ctrl to release, Right Ctrl+F fullscreen]"
        } else {
            "[Click to grab keyboard — Right Ctrl releases, Right Ctrl+F fullscreen]"
        };
        self.last_title = format!("{} {suffix}", self.title_prefix);
        if let Some(state) = self.state.as_mut() {
            state.title_prefix = self.last_title.clone();
            state.window.set_title(&self.last_title);
        }
    }
}

/// Clamp a resize dimension to even values within sane bounds.
fn clamp_resize_dim(v: u32) -> u32 {
    (v.max(320) & !1).min(7680)
}

/// HWND of the viewport window as a usize slot for the LL keyboard hook
/// (winit 0.30 exposes the raw window handle; read it through rwh 0.6).
fn window_hwnd_usable(window: &winit::window::Window) -> usize {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match window.window_handle() {
        Ok(handle) => match handle.as_raw() {
            RawWindowHandle::Win32(h) => h.hwnd.get() as usize,
            _ => 0,
        },
        Err(_) => 0,
    }
}

fn create_viewport(
    event_loop: &winit::event_loop::ActiveEventLoop,
    title_prefix: &str,
) -> anyhow::Result<ViewportState> {
    let attrs = winit::window::Window::default_attributes()
        .with_title(title_prefix)
        .with_inner_size(winit::dpi::LogicalSize::new(1280.0f64, 720.0f64));
    let window = Arc::new(event_loop.create_window(attrs)?);

    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::DX12,
        ..Default::default()
    });
    let surface = instance.create_surface(window.clone())?;
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
    }))?;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("wsldesk-viewport"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::default(),
        memory_hints: wgpu::MemoryHints::default(),
        trace: wgpu::Trace::Off,
    }))?;

    let size = window.inner_size();
    let caps = surface.get_capabilities(&adapter);
    // Prefer Bgra8Unorm (identity pass-through); the texture format is always
    // matched to whatever surface format we settle on.
    let surface_format = caps
        .formats
        .iter()
        .find(|f| **f == wgpu::TextureFormat::Bgra8Unorm)
        .or_else(|| caps.formats.first())
        .copied()
        .ok_or_else(|| anyhow::anyhow!("surface has no formats"))?;
    let surface_config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format: surface_format,
        width: size.width.max(1),
        height: size.height.max(1),
        present_mode: wgpu::PresentMode::AutoVsync,
        desired_maximum_frame_latency: 2,
        alpha_mode: caps.alpha_modes[0],
        view_formats: vec![],
    };
    surface.configure(&device, &surface_config);

    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("wsldesk-fullscreen"),
        source: wgpu::ShaderSource::Wgsl(FULLSCREEN_WGSL.into()),
    });
    let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("wsldesk-frame-layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("wsldesk-pipe-layout"),
        bind_group_layouts: &[&bind_group_layout],
        push_constant_ranges: &[],
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("wsldesk-pipe"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format: surface_format,
                blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    });
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("wsldesk-sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });

    Ok(ViewportState {
        window,
        device,
        queue,
        surface,
        surface_config,
        pipeline,
        bind_group_layout,
        sampler,
        texture: None,
        title_prefix: title_prefix.to_string(),
        fps_window_start: Instant::now(),
        fps_frames: 0,
    })
}

/// WGSL: fullscreen triangle sampling the captured frame texture.
const FULLSCREEN_WGSL: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> VsOut {
    // Fullscreen triangle covering clip space; uv over [0,2] clamps at edges.
    var out: VsOut;
    let xy = vec2<f32>(
        f32((idx << 1u) & 2u),
        f32(idx & 2u),
    );
    out.pos = vec4<f32>(xy * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
    out.uv = xy;
    return out;
}

@group(0) @binding(0) var frame_tex: texture_2d<f32>;
@group(0) @binding(1) var frame_samp: sampler;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Force opaque alpha: XRGB8888 padding bytes are undefined, and a
    // zero alpha byte would composite the frame against black (or make the
    // whole surface transparent on some compositors). The captured frame is
    // always opaque desktop content.
    let color = textureSample(frame_tex, frame_samp, in.uv);
    return vec4<f32>(color.rgb, 1.0);
}
"#;

/// Upload the newest frame (if any) and present one surface frame.
fn render_frame(state: &mut ViewportState, publisher: &FramePublisher) -> anyhow::Result<()> {
    let Some(frame) = publisher.try_take() else {
        return Ok(()); // nothing new — keep the last presented frame
    };
    let DecodedFrame { header, pixels } = frame;

    // Recreate the texture when resolution/format changes (sway resize,
    // session switch). Texture format matches the WIRE byte order; the
    // surface format matches the texture — raw pass-through, channels
    // never swap.
    let wire_format = match header.pixel_format {
        PixelFormat::Bgra8888 => wgpu::TextureFormat::Bgra8Unorm,
        PixelFormat::Rgba8888 => wgpu::TextureFormat::Rgba8Unorm,
    };
    let needs_recreate = match &state.texture {
        Some((_, _, w, h, fmt)) => *w != header.width || *h != header.height || *fmt != wire_format,
        None => true,
    };
    if needs_recreate {
        let texture = state.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("wsldesk-frame-texture"),
            size: wgpu::Extent3d {
                width: header.width,
                height: header.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            format: wire_format,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = state.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("wsldesk-frame-bind"),
            layout: &state.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&state.sampler),
                },
            ],
        });
        state.texture = Some((texture, bind_group, header.width, header.height, wire_format));
    }

    {
        let (texture, bind_group, _, _, _) = state.texture.as_ref().expect("texture just created");
        state.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(header.width * 4),
                rows_per_image: Some(header.height),
            },
            wgpu::Extent3d {
                width: header.width,
                height: header.height,
                depth_or_array_layers: 1,
            },
        );

        let surface_texture = match state.surface.get_current_texture() {
            Ok(texture) => texture,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                let size = state.window.inner_size();
                if size.width > 0 && size.height > 0 {
                    state.surface_config.width = size.width;
                    state.surface_config.height = size.height;
                    state.surface.configure(&state.device, &state.surface_config);
                }
                return Ok(());
            }
            Err(wgpu::SurfaceError::Timeout) => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = state
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("wsldesk-enc") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("wsldesk-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&state.pipeline);
            pass.set_bind_group(0, bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        state.queue.submit([encoder.finish()]);
        surface_texture.present();
    }

    // FPS + live title.
    state.fps_frames += 1;
    if state.fps_window_start.elapsed() >= Duration::from_secs(1) {
        let fps = state.fps_frames;
        let (w, h, _, _) = state
            .texture
            .as_ref()
            .map(|(_, _, w, h, _)| (*w, *h, 0u32, 0u32))
            .unwrap_or((0, 0, 0, 0));
        let title = format!("{} ({}x{} @ {} FPS, DX12)", state.title_prefix, w, h, fps);
        state.window.set_title(&title);
        state.fps_window_start = Instant::now();
        state.fps_frames = 0;
    }
    Ok(())
}

/// Entry point for the interactive viewport: runs the winit loop until the
/// window closes, then tears the session down.
///
/// The control connection is handed to a [`ControlPump`] (writer + reader
/// threads) so input forwarding never stalls DX12 rendering; the viewport's
/// idle timeout is infinite (an idle user must never lose the control
/// channel) and teardown is owned by window close / Ctrl+C / focus-loss.
pub fn run_viewport(
    distro: &'static str,
    title_prefix: &str,
    conn: HvConnection,
    publisher: Arc<FramePublisher>,
    fullscreen: bool,
) -> anyhow::Result<()> {
    let event_loop = winit::event_loop::EventLoop::new()?;
    let _ = publisher.wake.set(event_loop.create_proxy());
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(Mutex::new(VideoStats::default()));
    let output_size = Arc::new(Mutex::new((0u32, 0u32)));
    let conn = Arc::new(conn);
    let pump = Arc::new(ControlPump::start(conn.clone()));
    register_ctrl_handler(conn.clone());
    let rx_handle =
        spawn_video_receiver(distro, publisher.clone(), stop_flag.clone(), stats.clone());
    let clip_stop = Arc::new(AtomicBool::new(false));
    let _clip = input::spawn_clipboard_watcher(pump.clone(), clip_stop.clone());
    // Raw keyboard grab (Right Ctrl toggles; Super/Alt+Tab pass to Linux).
    let grab_enabled = Arc::new(AtomicBool::new(false));
    let hwnd = Arc::new(AtomicUsize::new(0));

    let mut app = ViewportApp {
        publisher: publisher.clone(),
        pump: pump.clone(),
        stop_flag: stop_flag.clone(),
        grab_enabled,
        hwnd,
        keyboard_hook: None,
        scroll_remainder: (0.0, 0.0),
        title_prefix: title_prefix.to_string(),
        output_size,
        state: None,
        pressed_keys: std::collections::HashSet::new(),
        right_ctrl_held: false,
        grab_chord_held: false,
        rctrl_hold_consumed: false,
        pending_resize: None,
        resize_deadline: None,
        fullscreen,
        last_title: title_prefix.to_string(),
        clipboard_hash: String::new(),
    };
    // Apply the --fullscreen flag before the loop starts.
    let result = event_loop.run_app(&mut app);

    // Wind everything down and report stats.
    publisher.close();
    stop_flag.store(true, Ordering::SeqCst);
    clip_stop.store(true, Ordering::SeqCst);
    app.keyboard_hook.take();
    let _ = rx_handle.join();
    pump.shutdown();
    if let Ok(stats) = stats.lock() {
        if stats.frames > 0 {
            println!(
                "video stats: {} frames @ {:.1} fps, {}x{}, wire {:.2} MB, compression {:.1}x",
                stats.frames,
                stats.frames as f64 / stats.seconds.max(1e-6),
                stats.width,
                stats.height,
                stats.wire_bytes as f64 / 1_048_576.0,
                stats.raw_bytes as f64 / stats.wire_bytes.max(1) as f64
            );
        }
    }
    result.map_err(|e| anyhow::anyhow!("event loop error: {e}"))
}

// ---------------------------------------------------------------------------
// `--headless-frames N` diagnostics (no window — CI/smoke verification)
// ---------------------------------------------------------------------------

/// Receive real frames directly (no window, no publisher), verifying each
/// is non-empty and correctly sized. Sequence (M4B verification):
/// 1. collect `pre_inject_frames` (default 3) at the requested geometry;
/// 2. call `inject` once (test InputEvent + live Resize);
/// 3. keep receiving until at least one frame arrives at the post-resize
///    target geometry — up to `max_frames` total (30) so normal mode-switch
///    latency can't cause a false negative.
///
/// Returns stats, the verified-frames count, and whether the live resize
/// flip was observed.
pub fn receive_headless_frames(
    distro: &str,
    pre_inject_frames: u64,
    resize_target: Option<(u32, u32)>,
    stop: &AtomicBool,
    inject: Option<&dyn Fn(u64)>,
) -> anyhow::Result<(VideoStats, u64, bool)> {
    let stream = connect_video_with_retry(distro, stop)?;
    eprintln!(
        "headless: connected to AF_VSOCK {VIDEO_PORT}, receiving {pre_inject_frames} frames, \
         then waiting for the live resize to land…"
    );
    let started = Instant::now();
    let max_frames = 30;
    let mut stats = VideoStats::default();
    let mut checksum_ok = 0u64;
    let mut saw_initial_geometry: Option<(u32, u32)> = None;
    let mut resize_flipped = false;
    let mut injected = false;
    while stats.frames < max_frames {
        if stop.load(Ordering::SeqCst) {
            anyhow::bail!("headless receive stopped early");
        }
        // Idle timeouts are transient (the sender caps at ~60 FPS but pauses
        // during output reconfiguration) — keep waiting while not stopped.
        let frame = match stream.recv_frame() {
            Ok(frame) => frame,
            Err(TransportError::Io(err))
                if err.kind() == std::io::ErrorKind::TimedOut && !stop.load(Ordering::SeqCst) =>
            {
                continue;
            }
            Err(err) => return Err(err.into()),
        };
        let expected_len = frame.header.width as usize * 4 * frame.header.height as usize;
        let nonzero = frame.pixels.iter().any(|&b| b != 0);
        if nonzero && frame.pixels.len() == expected_len {
            checksum_ok += 1;
        } else {
            eprintln!(
                "headless: frame {} suspect (len {} vs {expected_len}, non-zero content: {nonzero})",
                stats.frames,
                frame.pixels.len()
            );
        }
        stats.frames += 1;
        stats.raw_bytes += frame.pixels.len() as u64;
        stats.wire_bytes += frame.wire_len() as u64;
        stats.width = frame.header.width;
        stats.height = frame.header.height;
        stats.pixel_format = Some(frame.header.pixel_format);

        if stats.frames == pre_inject_frames {
            saw_initial_geometry = Some((frame.header.width, frame.header.height));
            if let Some(inject) = inject {
                eprintln!(
                    "headless: injecting test InputEvent + Resize after {} frames at {}x{}…",
                    stats.frames, frame.header.width, frame.header.height
                );
                inject(stats.frames);
                injected = true;
            }
        }

        // Post-inject: wait until the geometry flips to the resize target
        // (up to max_frames — mode changes take a few compositor cycles),
        // then hold ~2.5s so the guest's 2s input-telemetry window elapses
        // (proving input routing on the daemon log).
        if injected {
            if let Some(target) = resize_target {
                if (frame.header.width, frame.header.height) == target {
                    resize_flipped = true;
                    eprintln!(
                        "headless: live resize VERIFIED — frame {} arrived at {}x{}",
                        stats.frames, target.0, target.1
                    );
                    // Hold the session open so the guest's 2s telemetry
                    // window fires and the injected input is provably routed.
                    let hold_deadline = Instant::now() + Duration::from_millis(2500);
                    while Instant::now() < hold_deadline && stats.frames < max_frames + 60 {
                        if stop.load(Ordering::SeqCst) {
                            break;
                        }
                        match stream.recv_frame() {
                            Ok(f) => {
                                stats.frames += 1;
                                stats.raw_bytes += f.pixels.len() as u64;
                                stats.wire_bytes += f.wire_len() as u64;
                                stats.width = f.header.width;
                                stats.height = f.header.height;
                                stats.pixel_format = Some(f.header.pixel_format);
                            }
                            Err(TransportError::Io(err))
                                if err.kind() == std::io::ErrorKind::TimedOut =>
                            {
                                continue;
                            }
                            Err(err) => return Err(err.into()),
                        }
                    }
                    break;
                }
            } else {
                break; // no resize verification requested — enough frames
            }
        } else if resize_target.is_none() && stats.frames >= pre_inject_frames + 7 {
            break; // non-4B headless mode: stop after a few extra frames
        }
    }
    let _ = saw_initial_geometry;
    stats.seconds = started.elapsed().as_secs_f64();
    Ok((stats, checksum_ok, resize_flipped))
}
