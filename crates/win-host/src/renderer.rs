//! Native Win32 viewport: `winit` + `wgpu` (DX12) (Plan.Md §5 M4A).
//!
//! Data path: a dedicated blocking receiver thread pulls `b"WFRM"` frames
//! from the guest over `VIDEO_PORT`, LZ4-decodes them, and publishes the
//! updates into a bounded queue. Full frames supersede older work; damage
//! updates are applied in order or merged into a complete image. The winit
//! render loop uploads the pending regions via `queue.write_texture()` and
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

use common::protocol::{ControlMessage, PixelFormat, VideoFrameHeader};
use crate::input::{self, ControlPump};
use crate::transport::{DecodedFrame, FramePayload, HvConnection, TransportError, VideoStream};

/// Queue ordered damage until redraw. A complete image supersedes the queue.
pub struct FramePublisher {
    pending: Mutex<PendingFrames>,
    wake: OnceLock<winit::event_loop::EventLoopProxy<UserEvent>>,
    redraw_pending: AtomicBool,
    closed: AtomicBool,
}

#[derive(Default)]
struct PendingFrames {
    canvas: Option<(VideoFrameHeader, Vec<u8>)>,
    frames: Vec<DecodedFrame>,
    rect_count: usize,
    dirty_bytes: usize,
}

impl PendingFrames {
    fn push(&mut self, frame: DecodedFrame) -> anyhow::Result<()> {
        let header = frame.header;
        match &frame.payload {
            FramePayload::Full(pixels) => {
                self.canvas = Some((header, pixels.clone()));
                self.frames.clear();
                self.rect_count = 0;
                self.dirty_bytes = 0;
                self.frames.push(frame);
            }
            FramePayload::DirtyRects(rects) => {
                let (base, canvas) = self.canvas.as_mut()
                    .ok_or_else(|| anyhow::anyhow!("damage frame arrived before a full frame"))?;
                anyhow::ensure!(
                    (base.width, base.height, base.pixel_format) == (header.width, header.height, header.pixel_format),
                    "damage frame changed layout without a full frame"
                );
                for rect in rects {
                    let region = rect.region;
                    let row_bytes = region.width as usize * 4;
                    for (row, pixels) in rect.pixels.chunks_exact(row_bytes).enumerate() {
                        let start = (region.y as usize + row) * header.stride as usize + region.x as usize * 4;
                        canvas[start..start + row_bytes].copy_from_slice(pixels);
                    }
                }
                self.rect_count += rects.len();
                self.dirty_bytes += frame.pixel_len();
                // Bound both the number of rectangle uploads and their memory.
                // The CPU canvas includes every previous delta, including those
                // the renderer may already have uploaded.
                if self.rect_count > 64 || self.dirty_bytes > canvas.len() {
                    self.frames.clear();
                    self.rect_count = 0;
                    self.dirty_bytes = 0;
                    self.frames.push(DecodedFrame {
                        header: VideoFrameHeader { rect_count: 0, ..header },
                        payload: FramePayload::Full(canvas.clone()),
                    });
                } else {
                    self.frames.push(frame);
                }
            }
        }
        Ok(())
    }
}

enum UserEvent {
    FrameReady,
    VideoClosed,
}

impl FramePublisher {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(PendingFrames::default()),
            wake: OnceLock::new(),
            redraw_pending: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        }
    }

    /// Queue the frame and wake the render thread once per pending batch.
    pub fn publish(&self, frame: DecodedFrame) -> anyhow::Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        let should_wake = {
            let mut pending = self.pending.lock().expect("frame lock");
            pending.push(frame)?;
            !self.redraw_pending.swap(true, Ordering::Relaxed)
        };
        if should_wake {
            self.wake_event_loop(UserEvent::FrameReady);
        }
        Ok(())
    }

    /// Take the pending updates in stream order.
    pub fn take_updates(&self) -> Vec<DecodedFrame> {
        let mut pending = self.pending.lock().expect("frame lock");
        self.redraw_pending.store(false, Ordering::Relaxed);
        pending.rect_count = 0;
        pending.dirty_bytes = 0;
        std::mem::take(&mut pending.frames)
    }

    /// True once the receiver wants the render loop to wind down.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.wake_event_loop(UserEvent::VideoClosed);
    }

    fn wake_event_loop(&self, event: UserEvent) {
        if let Some(proxy) = self.wake.get() {
            let _ = proxy.send_event(event);
        }
    }

    /// Check whether the live test has received its first frame.
    #[cfg(test)]
    pub fn peek(&self) -> bool {
        !self.pending.lock().expect("frame lock").frames.is_empty()
    }
}

/// Live session stats printed by the receiver thread when it finishes.
#[derive(Debug, Default, Clone)]
pub struct VideoStats {
    pub frames: u64,
    pub full_frames: u64,
    pub dirty_frames: u64,
    pub dirty_wire_bytes: u64,
    pub seconds: f64,
    pub wire_bytes: u64,
    pub raw_bytes: u64,
    pub width: u32,
    pub height: u32,
    pub pixel_format: Option<PixelFormat>,
}

impl VideoStats {
    fn record(&mut self, frame: &DecodedFrame) {
        self.frames += 1;
        self.wire_bytes += frame.wire_len() as u64;
        self.raw_bytes += frame.pixel_len() as u64;
        self.width = frame.header.width;
        self.height = frame.header.height;
        self.pixel_format = Some(frame.header.pixel_format);
        match frame.payload {
            FramePayload::Full(_) => self.full_frames += 1,
            FramePayload::DirtyRects(_) => {
                self.dirty_frames += 1;
                self.dirty_wire_bytes += frame.wire_len() as u64;
            }
        }
    }
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
                if !stream.wait_readable(Duration::from_millis(100)) {
                    continue;
                }
                match stream.recv_frame() {
                    Ok(frame) => {
                        local.record(&frame);
                        if let Err(err) = publisher.publish(frame) {
                            eprintln!("video receiver: {err}");
                            break;
                        }
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
    samplers: [wgpu::Sampler; 2],
    texture: Option<(wgpu::Texture, [wgpu::BindGroup; 2], u32, u32, wgpu::TextureFormat)>,
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
    last_resize: (u32, u32, u32),
    refresh_mhz: u32,
    default_refresh: bool,
    resize_deadline: Option<Instant>,
    fullscreen: bool,
    last_title: String,
    clipboard_hash: String,
}

impl winit::application::ApplicationHandler<UserEvent> for ViewportApp {
    fn user_event(&mut self, event_loop: &winit::event_loop::ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::FrameReady => {
                if let Some(state) = &self.state {
                    state.window.request_redraw();
                }
            }
            UserEvent::VideoClosed => {
                self.teardown();
                event_loop.exit();
            }
        }
    }

    fn resumed(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        let (width, height, _) = self.last_resize;
        match create_viewport(event_loop, &self.title_prefix, width, height) {
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
                if self.default_refresh {
                    if let Some(refresh) = state.window.current_monitor()
                        .and_then(|monitor| monitor.refresh_rate_millihertz())
                    {
                        eprintln!("DX12 monitor: {refresh} mHz; guest default: {} mHz (override with --fps)", self.refresh_mhz);
                    }
                }
                let size = state.window.inner_size();
                let scale = state.window.scale_factor();
                state.window.request_redraw();
                self.state = Some(state);
                self.queue_resize(size, scale);
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
            winit::event::WindowEvent::Resized(_) | winit::event::WindowEvent::ScaleFactorChanged { .. } => {
                let (size, scale) = match event {
                    winit::event::WindowEvent::Resized(size) => (size, state.window.scale_factor()),
                    winit::event::WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                        (state.window.inner_size(), scale_factor)
                    }
                    _ => unreachable!(),
                };
                if size.width > 0 && size.height > 0 {
                    // Debounce: reconfigure the swapchain immediately, but
                    // only forward to the guest after ~150ms of quiet, with
                    // even, sane dimensions and the real monitor scale.
                    if (state.surface_config.width, state.surface_config.height) != (size.width, size.height) {
                        state.surface_config.width = size.width;
                        state.surface_config.height = size.height;
                        state.surface.configure(&state.device, &state.surface_config);
                    }
                    state.window.request_redraw();
                    self.queue_resize(size, scale);
                }
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

    fn about_to_wait(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
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
                    refresh_mhz: self.refresh_mhz,
                });
                self.last_resize = (w, h, scale);
                self.pending_resize = None;
                self.resize_deadline = None;
            }
        }
        use winit::event_loop::ControlFlow;
        event_loop.set_control_flow(self.resize_deadline.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }
}

impl ViewportApp {
    fn queue_resize(&mut self, size: winit::dpi::PhysicalSize<u32>, scale: f64) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        let mode = (
            size.width.clamp(320, 7680) & !1,
            size.height.clamp(240, 7680) & !1,
            (scale * 1000.0).round() as u32,
        );
        if mode == self.last_resize {
            self.pending_resize = None;
            self.resize_deadline = None;
        } else if self.pending_resize != Some(mode) {
            self.pending_resize = Some(mode);
            self.resize_deadline = Some(Instant::now() + Duration::from_millis(150));
        }
    }
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
    width: u32,
    height: u32,
) -> anyhow::Result<ViewportState> {
    let attrs = winit::window::Window::default_attributes()
        .with_title(title_prefix)
        .with_inner_size(winit::dpi::PhysicalSize::new(width, height));
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
    let present_mode = [
        wgpu::PresentMode::Mailbox,
        wgpu::PresentMode::AutoNoVsync,
        wgpu::PresentMode::Immediate,
        wgpu::PresentMode::Fifo,
    ].into_iter().find(|mode| caps.present_modes.contains(mode)).unwrap_or(wgpu::PresentMode::Fifo);
    eprintln!("DX12 present mode: {present_mode:?}");
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
        present_mode,
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
    let samplers = [wgpu::FilterMode::Nearest, wgpu::FilterMode::Linear].map(|filter| device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("wsldesk-sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: filter,
        min_filter: filter,
        ..Default::default()
    }));

    Ok(ViewportState {
        window,
        device,
        queue,
        surface,
        surface_config,
        pipeline,
        bind_group_layout,
        samplers,
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

fn upload_updates(state: &mut ViewportState, updates: Vec<DecodedFrame>) -> anyhow::Result<()> {
    let Some(last) = updates.last() else { return Ok(()); };
    let header = last.header;

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
        anyhow::ensure!(
            matches!(updates.first().map(|frame| &frame.payload), Some(FramePayload::Full(_))),
            "new texture layout requires a full frame"
        );
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
        let bind_groups = state.samplers.each_ref().map(|sampler| state.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("wsldesk-frame-bind"),
            layout: &state.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        }));
        state.texture = Some((texture, bind_groups, header.width, header.height, wire_format));
    }

    let (texture, ..) = state.texture.as_ref().expect("texture initialized");
    let write = |x, y, width, height, pixels: &[u8]| {
        state.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
    };
    for update in updates {
        match update.payload {
            FramePayload::Full(pixels) => write(0, 0, header.width, header.height, &pixels),
            FramePayload::DirtyRects(rects) => {
                for rect in rects {
                    let region = rect.region;
                    write(region.x, region.y, region.width, region.height, &rect.pixels);
                }
            }
        }
    }
    Ok(())
}

/// Apply all pending deltas, then present once.
fn render_frame(state: &mut ViewportState, publisher: &FramePublisher) -> anyhow::Result<()> {
    upload_updates(state, publisher.take_updates())?;
    let Some((_, bind_groups, width, height, _)) = state.texture.as_ref() else { return Ok(()); };
    {
        let surface_texture = match state.surface.get_current_texture() {
            Ok(texture) => texture,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                let size = state.window.inner_size();
                if size.width > 0 && size.height > 0 {
                    state.surface_config.width = size.width;
                    state.surface_config.height = size.height;
                    state.surface.configure(&state.device, &state.surface_config);
                    state.window.request_redraw();
                }
                return Ok(());
            }
            Err(wgpu::SurfaceError::Timeout) => {
                if !state.window.is_minimized().unwrap_or(false) {
                    state.window.request_redraw();
                }
                return Ok(());
            }
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
            let scaling = (*width, *height) != (state.surface_config.width, state.surface_config.height);
            pass.set_bind_group(0, &bind_groups[usize::from(scaling)], &[]);
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
#[allow(clippy::too_many_arguments)]
pub fn run_viewport(
    distro: &'static str,
    title_prefix: &str,
    conn: HvConnection,
    publisher: Arc<FramePublisher>,
    fullscreen: bool,
    initial_mode: (u32, u32, u32),
    refresh_mhz: u32,
    default_refresh: bool,
) -> anyhow::Result<()> {
    let event_loop = winit::event_loop::EventLoop::<UserEvent>::with_user_event().build()?;
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
        last_resize: initial_mode,
        refresh_mhz,
        default_refresh,
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

/// Drive cursor damage without a viewport and verify both layouts.
pub fn receive_headless_frames(
    distro: &str,
    sample_frames: u64,
    initial_size: (u32, u32),
    scale_milli: u32,
    refresh_mhz: u32,
    conn: &HvConnection,
    stop: &AtomicBool,
) -> anyhow::Result<(VideoStats, u64, bool)> {
    use common::protocol::InputPayload;

    let stream = connect_video_with_retry(distro, stop)?;
    let resize_target = (800, 600);
    let started = Instant::now();
    let deadline = started + Duration::from_secs(30);
    let mut stats = VideoStats::default();
    let mut sample = VideoStats::default();
    let mut base = None;
    let mut first_frame_at = None;
    let mut sample_started_at = None;
    let mut resize_sent_at = None;
    let mut resized_at = None;
    let mut motion_at = started;
    let motion_period = Duration::from_nanos(1_000_000_000_000 / u64::from(refresh_mhz));
    let mut motion = 0u32;

    eprintln!("headless: measuring {sample_frames} frames with synthetic cursor motion");
    while Instant::now() < deadline {
        anyhow::ensure!(!stop.load(Ordering::SeqCst), "headless receive stopped early");
        let now = Instant::now();
        if let Some(at) = resized_at {
            if now - at >= Duration::from_millis(2500) {
                break;
            }
        } else if resize_sent_at.is_some_and(|at| now - at >= Duration::from_secs(5)) {
            break;
        }
        if now >= motion_at {
            let (width, height, _) = base.unwrap_or((initial_size.0, initial_size.1, PixelFormat::Bgra8888));
            motion = motion.wrapping_add(11);
            conn.send_msg(&ControlMessage::InputEvent(InputPayload::PointerMotionAbsolute {
                x: width / 4 + motion % (width / 2).max(1),
                y: height / 2,
                width,
                height,
            }))?;
            motion_at = now + motion_period;
        }
        // No packets are expected while the scene is idle. Wait for the
        // first byte before entering a framed read with the socket timeout.
        if !stream.wait_readable(Duration::from_millis(2)) {
            continue;
        }
        let frame = stream.recv_frame()?;
        let geometry = (frame.header.width, frame.header.height);
        let layout = (geometry.0, geometry.1, frame.header.pixel_format);
        if matches!(frame.payload, FramePayload::Full(_)) {
            base = Some(layout);
        } else {
            anyhow::ensure!(base == Some(layout), "dirty frame without a matching full frame");
        }

        let now = Instant::now();
        first_frame_at.get_or_insert(now);
        stats.record(&frame);
        if resize_sent_at.is_none() && geometry == initial_size {
            let first = *sample_started_at.get_or_insert(now);
            sample.record(&frame);
            if sample.frames == sample_frames {
                let fps = sample.frames.saturating_sub(1) as f64 / (now - first).as_secs_f64().max(0.000_001);
                eprintln!(
                    "headless: initial {}x{}: {:.1} FPS, {} full / {} dirty, mean dirty frame {:.0} wire bytes",
                    geometry.0, geometry.1, fps, sample.full_frames, sample.dirty_frames,
                    sample.dirty_wire_bytes as f64 / sample.dirty_frames.max(1) as f64,
                );
                if geometry == resize_target {
                    break;
                }
                conn.send_msg(&ControlMessage::Resize {
                    width: resize_target.0,
                    height: resize_target.1,
                    scale_milli,
                    refresh_mhz,
                })?;
                resize_sent_at = Some(now);
            }
        } else if resize_sent_at.is_some() && resized_at.is_none() && geometry == resize_target {
            anyhow::ensure!(frame.header.rect_count == 0, "resize did not have a full base frame");
            resized_at = Some(now);
            eprintln!("headless: live resize VERIFIED — full frame {} at 800x600", stats.frames);
        }
    }
    anyhow::ensure!(sample.frames >= sample_frames, "not enough frames at the requested size before the headless deadline");
    stats.seconds = first_frame_at.unwrap_or(started).elapsed().as_secs_f64();
    let validated = stats.frames;
    Ok((stats, validated, resized_at.is_some()))
}

#[cfg(test)]
mod damage_tests {
    use super::*;
    use crate::transport::DirtyRect;
    use common::protocol::{DirtyRectHeader, FrameCompression};

    #[test]
    fn queue_overflow_preserves_disjoint_and_overlapping_updates() {
        let header = VideoFrameHeader {
            frame_id: 0, width: 80, height: 1, stride: 320, pixel_format: PixelFormat::Bgra8888,
            compression: FrameCompression::Raw, rect_count: 0, payload_len: 320,
        };
        let publisher = FramePublisher::new();
        publisher.publish(DecodedFrame { header, payload: FramePayload::Full(vec![0; 320]) }).unwrap();
        assert_eq!(publisher.take_updates().len(), 1);
        for index in 0..65 {
            let x = index % 64;
            publisher.publish(DecodedFrame {
                header: VideoFrameHeader { frame_id: u64::from(index + 1), rect_count: 1, ..header },
                payload: FramePayload::DirtyRects(vec![DirtyRect {
                    region: DirtyRectHeader { x, y: 0, width: 1, height: 1 },
                    pixels: vec![index as u8 + 1; 4],
                }]),
            }).unwrap();
        }
        let updates = publisher.take_updates();
        assert_eq!(updates.len(), 1);
        let FramePayload::Full(canvas) = &updates[0].payload else { panic!("expected compacted full frame") };
        assert_eq!(&canvas[..4], &[65; 4]);
        assert_eq!(&canvas[4..8], &[2; 4]);
        assert_eq!(&canvas[252..256], &[64; 4]);
        assert_eq!(&canvas[256..], &[0; 64]);
    }
}
