

Markdown
# Project Specification: WSLDesk
**Distro-Agnostic, Native Wayland Full-Desktop Bridge for WSL2 (Rust)**

---

## 1. Vision & End-State Architecture

WSLDesk replaces legacy X11/VNC/XRDP stacks (such as Win-KeX) with a modern, zero-network, Wayland-first full-desktop bridge between Windows 11 and any WSL2 distribution (Arch, Fedora, Ubuntu, Debian, Rocky Linux, openSUSE, NixOS).

### The End Goal ("Path B-Lite": Rooted Native Windows Renderer)
Instead of trying to re-implement 20+ Wayland desktop shell protocols (`xdg-shell`, `wl_subsurface`, popups) natively in Win32—or falling back to sluggish Xorg/VNC/RDP—WSLDesk uses a **Rooted Native Renderer** pipeline:

```text
┌────────────────────────────────────────────────────────────────────────────┐
│ WSL2 Linux Guest (Any Distro, systemd=true, /dev/dxg vGPU)                 │
│                                                                            │
│  [Wayland Compositor: GNOME / KDE / Sway / Hyprland / COSMIC / Weston]     │
│     │  (Renders on Windows GPU via /dev/dxg + Mesa D3D12 Gallium)          │
│     ▼                                                                      │
│  [wsldesk-guest] (Single static x86_64-unknown-linux-musl binary)          │
│     ├─ Mount/Env Isolator (private /tmp/.X11-unix, clean XDG_RUNTIME_DIR)  │
│     ├─ Pure-Rust Wayland Capture (wlr-screencopy / ext-image-copy-capture) │
│     ├─ Pure-Rust Input Injector (virtual-keyboard/pointer / EIS / D-Bus)   │
│     └─ LZ4 Dirty-Tile Frame Differ + AF_VSOCK Server (Ports 50001 / 50002) │
└───────────────────────────────┬────────────────────────────────────────────┘
                                │  Hyper-V VMBus Shared Memory
                                │  (AF_VSOCK ↔ AF_HYPERV, Zero TCP/IP, Zero Firewall)
┌───────────────────────────────▼────────────────────────────────────────────┐
│ Windows 11 Host                                                            │
│                                                                            │
│  [wsldesk.exe] (Single native x86_64-pc-windows-msvc binary)               │
│     ├─ Provisioner & Doctor (.wslconfig, /etc/wsl.conf, SHA-256 deployer)  │
│     ├─ AF_HYPERV Client (connects via `wslinfo --vm-id`, NO Admin needed)  │
│     ├─ Native Viewport (`winit` + `wgpu` Direct3D 12 Swapchain)            │
│     └─ Raw Input Grab (Super/Alt+Tab) + Bidirectional Win32 Clipboard      │
└────────────────────────────────────────────────────────────────────────────┘
Hard Architectural Constraints
Two Binaries Total:

Windows Host (wsldesk.exe): x86_64-pc-windows-msvc. Handles WSL2 setup, config patching, guest deployment, AF_HYPERV transport, and the native Direct3D 12 viewport.

Linux Guest (wsldesk-guest): x86_64-unknown-linux-musl (100% statically linked). Deployed automatically into /usr/local/bin/wsldesk-guest. Zero glibc or distro shared-library dependencies.

Zero-Network, Zero-Admin Transport:

Never use WSL2 NAT IPs, localhost port forwarding, or Windows Firewall rules.

Use Hyper-V Sockets (AF_VSOCK ↔ AF_HYPERV).

Connection Direction Rule (CRITICAL): The Linux guest MUST act as the AF_VSOCK listener (VMADDR_CID_ANY), and the Windows host MUST act as the AF_HYPERV client (connect()) using the VM GUID from wsl.exe -d <distro> -- wslinfo --vm-id.

Why: Windows connecting into a WSL2 VM via HV_GUID_VSOCK_TEMPLATE requires zero Administrator privileges and zero Windows Registry edits (GuestCommunicationServices). Reversing the direction causes WSAEACCES (10013) on non-elevated shells.

WSLg Coexistence:

Never destroy normal WSL2 terminal workflows. Isolate desktop sessions inside a private mount namespace (unshare) and isolated XDG_RUNTIME_DIR so WSLg's /tmp/.X11-unix (:0) and wayland-0 sockets never collide with the full desktop compositor.

2. Static Linking & Library Architecture (READ BEFORE CODING)
Because wsldesk-guest is compiled as a static musl binary, C-FFI libraries that depend on glibc or runtime dlopen() plugins (like libwayland-client.so or libpipewire-0.3.so) WILL BREAK static linking if used carelessly. Follow these exact crate choices and linking rules:

A. Shared Wire Protocol (crates/common — Pure Rust, OS-Agnostic)
Crate	Purpose	Linking Notes
serde (derive)	Control message serialization	Pure Rust
postcard (alloc)	Compact, deterministic binary encoding for control messages	Pure Rust; faster & safer than bincode
lz4_flex	Fast dirty-tile / frame compression (Milestone 4)	Pure Rust (no_std/safe); no C liblz4 needed
thiserror	Typed protocol & framing errors	Pure Rust
Zero-Copy Frame Rule: NEVER serialize raw pixel buffers (e.g., 8MB RGBA frames) through serde/postcard. Use postcard only for small control messages and fixed headers; read/write raw pixel bytes directly from/to the socket buffer.

B. Windows Host (crates/win-host — x86_64-pc-windows-msvc)
Crate	Purpose	Required Features / Linking Notes
clap (derive)	CLI subcommands (setup, doctor, ping, sessions, launch)	Static
tokio (full)	Async I/O, process orchestration, dual-channel socket pumps	Static
anyhow / tracing	Error context & structured logging	Static
sha2 / hex	SHA-256 verification of deployed wsldesk-guest binary	Pure Rust
windows (v0.58+)	Win32 AF_HYPERV (SOCKADDR_HV), clipboard, optional WH_KEYBOARD_LL hook	Features: Win32_Networking_WinSock, Win32_Foundation, Win32_System_DataExchange, Win32_UI_WindowsAndMessaging. Links cleanly against standard Windows system DLLs (ws2_32.dll, user32.dll).
winit (v0.30+)	[M4] Native Win32 windowing, HiDPI scaling, raw keyboard/mouse events	Pure Rust Win32 backend
wgpu (v22+)	[M4] Direct3D 12 GPU texture upload & fullscreen quad presentation	Explicitly prefer Backends::DX12
C. Linux Guest (crates/wsl-guest — x86_64-unknown-linux-musl)
Crate	Purpose	Linking Notes (CRITICAL FOR STATIC MUSL)
clap (derive)	Guest CLI (--version, doctor-probe, daemon, list-sessions, exec-session)	Pure Rust
tokio (rt-multi-thread, net, io-util, process, sync)	Async runtime for VSOCK server & session supervision	Pure Rust
vsock + tokio-vsock	Linux AF_VSOCK listener (VMADDR_CID_ANY)	Pure Rust syscalls (libc)
nix	unshare(CLONE_NEWNS), mount(), setresuid, signal handling	Features: sched, mount, user, process, signal
wayland-client + wayland-protocols + wayland-protocols-wlr + wayland-protocols-misc	[M4] Headless Wayland screen capture (zwlr_screencopy_manager_v1 / ext_image_copy_capture_v1) & virtual input (zwp_virtual_keyboard_manager_v1, zwlr_virtual_pointer_manager_v1)	CRITICAL: Keep default features (DO NOT enable the system feature!). Without system, wayland-rs uses its 100% pure-Rust wire protocol implementation over AF_UNIX + SCM_RIGHTS, requiring zero C libraries (libwayland-client.so is NOT needed).
zbus	[M4] D-Bus client for org.freedesktop.portal.ScreenCast / RemoteDesktop (GNOME/KDE)	100% Pure Rust; zero libdbus C dependency
reis	[M4] Emulated Input Server (EIS / libei) client for GNOME/KDE input injection	100% Pure Rust implementation of libei
PipeWire Boundary Note (Milestone 4): pipewire-rs links dynamically to libpipewire-0.3.so (glibc) and relies on runtime dlopen() of SPA plugins, which is incompatible with static musl binaries. Therefore:

Tier 1 (Pure Static Musl — Default): wlr-screencopy / ext-image-copy-capture + virtual-keyboard / virtual-pointer works statically out of the box on Sway, Hyprland, River, Wayfire, Labwc, COSMIC, and nested Weston.

Tier 2 (GNOME / KDE ScreenCast): Keep wsldesk-guest statically linked; for PipeWire streams, either bridge via a minimal distro-native helper (gst-launch-1.0 pipewiresrc / tiny glibc sidecar) or orchestrate native gnome-remote-desktop / krdp over the same AF_VSOCK tunnel.

3. Cargo Workspace Layout & Cross-Target Compilation Rules
Workspace Structure
Plaintext
wsldesk/
├── Cargo.toml                  # Workspace manifest (resolver = "2")
├── PLAN.md                     # This specification
├── AGENTS.md                   # Strict agent guardrails & compile-check commands
├── crates/
│   ├── common/                 # OS-agnostic protocol, framing, shared DTOs
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── protocol.rs     # ControlMessage, FrameHeader, Magic/Version constants
│   │       └── keymap.rs       # [M4] Cross-platform evdev keycode definitions
│   ├── win-host/               # Windows binary (`wsldesk.exe`)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── main.rs         # CLI entry (`setup`, `doctor`, `ping`, `sessions`, `launch`)
│   │       ├── wsl_cli.rs      # `wsl.exe` wrapper (UTF-16LE & UTF-8 safe)
│   │       ├── config.rs       # Non-destructive INI patcher (.wslconfig & /etc/wsl.conf)
│   │       ├── deployer.rs     # Atomic SHA-256 guest binary deployer & systemd unit installer
│   │       ├── transport.rs    # [M2] Win32 `AF_HYPERV` (`SOCKADDR_HV`) async client
│   │       └── renderer.rs     # [M4] `winit` + `wgpu` (D3D12) viewport, input grab, clipboard
│   └── wsl-guest/              # Linux static binary (`wsldesk-guest`)
│       ├── Cargo.toml
│       └── src/
│           ├── main.rs         # Guest CLI & daemon entry
│           ├── probe.rs        # Systemd PID 1, /dev/dxg, Mesa, Wayland session diagnostics
│           ├── transport.rs    # [M2] Linux `AF_VSOCK` async listener
│           ├── env_isolate.rs  # [M3] Mount namespace & WSLg socket isolation
│           ├── sessions.rs     # [M3] `/usr/share/wayland-sessions/*.desktop` parser & launcher
│           └── capture.rs      # [M4] Pure-Rust Wayland frame capture, LZ4 tile diff, input inject
Preventing Cross-OS cargo check Failures
Because Windows cannot compile vsock/nix and Linux cannot compile Win32 windows crates:

In crates/win-host/Cargo.toml, place all Windows-specific crates under [target.'cfg(windows)'.dependencies].

In crates/wsl-guest/Cargo.toml, place all Linux-specific crates under [target.'cfg(target_os = "linux")'.dependencies].

In both main.rs files, gate the real implementation behind #[cfg(windows)] / #[cfg(target_os = "linux")] and provide a tiny compile-time stub for the opposite OS (#[cfg(not(...))] fn main() { panic!("Target OS mismatch"); }).

Result: cargo check --workspace succeeds cleanly on both Windows and Linux, and rust-analyzer stays green in VS Code regardless of which side is open.

4. Detailed Technical Specifications
4.1 Wire Protocol (crates/common/src/protocol.rs)
Define two dedicated VSOCK ports:

CONTROL_PORT: u32 = 50001 — Bidirectional framed control plane.

VIDEO_PORT: u32 = 50002 — Unidirectional high-throughput frame stream (Guest → Host).

Control Frame Wire Format (Little-Endian):

Plaintext
┌───────────────────┬───────────────────┬───────────────────────────────┐
│ Magic: [u8; 4]    │ Length: u32 (LE)  │ Payload: [u8; Length]         │
│ b"WSLD"           │ Max 16 MiB        │ postcard-encoded ControlMsg   │
└───────────────────┴───────────────────┴───────────────────────────────┘
Core ControlMessage Enum (extend across milestones):

Hello { protocol_version: u32, host_version: String }

Welcome { protocol_version: u32, guest_version: String, has_dxg: bool, systemd_active: bool }

Ping { seq: u64, timestamp_us: u64 } / Pong { seq: u64, timestamp_us: u64 }

ListSessions / SessionList(Vec<WaylandSessionInfo>)

StartSession { session_id: String, width: u32, height: u32, scale_milli: u32 }

Resize { width: u32, height: u32, scale_milli: u32 }

InputEvent(InputPayload) (Key, PointerMotion, PointerButton, PointerAxis)

ClipboardUpdate { mime_type: String, data: Vec<u8> }

Error { code: u32, message: String }

Video Frame Wire Format (Milestone 4 — Raw Zero-Copy, No Serde on Pixels):

Plaintext
Fixed 32-byte Header (Little-Endian):
- magic: [u8; 4]        // b"WFRM"
- frame_id: u64
- width: u32
- height: u32
- stride: u32
- pixel_format: u8      // 0 = BGRA8888, 1 = RGBA8888
- compression: u8       // 0 = Raw, 1 = LZ4
- rect_count: u16       // Number of dirty tiles following header (0 = full frame)
- payload_len: u32      // Exact byte length of compressed/raw pixel payload
Followed immediately by `payload_len` raw bytes.
4.2 Win32 AF_HYPERV Socket Binding (crates/win-host/src/transport.rs)
Reference: ALinuxPerson/hv_sock, microsoft/WSL#5751, marchaesen/vcxsrv#80.

Address Family: AF_HYPERV = 34

Socket Type: SOCK_STREAM

Protocol: HV_PROTOCOL_RAW = 1

HV_GUID_VSOCK_TEMPLATE: 00000000-facb-11e6-bd58-64006a7986d3

Construct ServiceId by setting Data1 = port (e.g., 50001u32) and copying the remaining template bytes.

VmId: Execute wsl.exe -d <distro> -- wslinfo --vm-id, trim whitespace and {} braces, parse into a Win32 GUID.

Memory Safety Rule: Zero-initialize SOCKADDR_HV completely (Reserved = 0) before passing its pointer and size_of::<SOCKADDR_HV>() to WinSock::connect().

Wrap the connected Win32 SOCKET carefully for async I/O (or run blocking recv/send on a dedicated tokio::task::spawn_blocking pump with bounded tokio::sync::mpsc channels to avoid Win32 IOCP edge cases with AF_HYPERV).

4.3 Handling wsl.exe Encoding & Atomic Deployment (wsl_cli.rs & deployer.rs)
The UTF-16LE Trap: wsl.exe --list --verbose and wsl.exe --version output UTF-16LE by default on Windows.

Always set .env("WSL_UTF8", "1") on std::process::Command::new("wsl.exe").

AND implement a defensive decoder decode_wsl_output(bytes: &[u8]) -> String that checks for a UTF-16LE BOM (0xFF 0xFE) or interleaved 0x00 bytes and decodes as UTF-16LE if detected, falling back to lossy UTF-8.

Atomic Guest Deployment (No UNC /mnt/c Dependency):

Never assume automount=true or \\wsl$\ paths are enabled.

Compute local SHA-256 of wsldesk-guest.

Query remote SHA-256 via wsl.exe -d <distro> -u root -- sh -c 'sha256sum /usr/local/bin/wsldesk-guest 2>/dev/null || true'.

If missing or mismatched, stream the binary bytes directly into stdin of:

Bash
wsl.exe -d <distro> -u root -- sh -c 'cat > /usr/local/bin/wsldesk-guest.tmp && chmod 0755 /usr/local/bin/wsldesk-guest.tmp && mv -f /usr/local/bin/wsldesk-guest.tmp /usr/local/bin/wsldesk-guest'
This guarantees atomic replacement (never fails with ETXTBSY if an old instance is running) and works on minimal distros lacking bash, curl, or python.

5. Phased Execution Milestones
CRITICAL AGENT RULE: Implement ONE milestone at a time. Do not write Milestone 4 wgpu renderer code while implementing Milestone 1. Every milestone must compile cleanly (cargo check --workspace + cargo clippy) and pass its verification command before advancing.

Milestone 1: Windows Bootstrapper, Safe Config Manager, Guest Deployer & Doctor (START HERE)
Goal: A user runs wsldesk.exe setup --distro <name> and wsldesk.exe doctor --distro <name> to fully provision Windows + WSL2 + the static guest binary.

Deliverables:

Workspace & common Crate:

Initialize Cargo.toml, AGENTS.md, crates/common, crates/win-host, crates/wsl-guest.

Implement ControlMessage enum and length-prefixed framing (encode_frame / decode_frame) with unit tests in crates/common.

wsl_cli.rs (Windows):

check_wsl_version(): Verify Store WSL 2.0+ is installed.

list_distros(): Parse wsl.exe --list --verbose into Vec<DistroInfo { name, state, version, is_default }>.

install_distro(name): Trigger wsl.exe --install -d <name> --no-launch if requested via --install.

get_vm_id(distro): Run wsl.exe -d <distro> -- wslinfo --vm-id and parse clean GUID string.

exec_root(distro, sh_cmd, stdin_bytes) & exec_user(distro, args).

config.rs (Non-Destructive INI Patcher):

Custom line-preserving INI editor (do NOT use an INI serializer that strips comments or scrambles user keys!).

Windows %USERPROFILE%\.wslconfig: Create timestamped backup (.wslconfig.wsldesk.<unix_ts>.bak) before writing. Ensure [wsl2] exists; optionally toggle guiApplications=false ONLY when --disable-wslg is explicitly passed. Log exact line diff.

Linux /etc/wsl.conf: Read via wsl.exe -d <distro> -u root -- cat /etc/wsl.conf. Backup to /etc/wsl.conf.wsldesk.<ts>.bak inside the distro. Ensure [boot] contains systemd=true. If modified, notify user and run wsl.exe --terminate <distro> so systemd boots cleanly on next launch.

deployer.rs (Static Guest Deployment):

Resolve wsldesk-guest binary path via --guest-bin <path>, sidecar next to wsldesk.exe, or target/x86_64-unknown-linux-musl/release/wsldesk-guest.

Perform SHA-256 check and atomic stdin deployment to /usr/local/bin/wsldesk-guest.

Verify execution via wsldesk-guest --version.

Optionally install a user systemd service (/etc/systemd/user/wsldesk-guest.service) when --enable-service is passed.

wsl-guest Minimal Binary (probe.rs):

Implements --version and doctor-probe --json, returning structured JSON:

pid1_comm (verifies systemd)

has_dxg (/dev/dxg exists and is readable)

wslg_detected (/mnt/wslg mounted)

wayland_sessions (parsed .desktop files from /usr/share/wayland-sessions/)

mesa_d3d12_available (checks /usr/lib*/dri/d3d12_dri.so or glxinfo/vulkaninfo availability)

wsldesk.exe doctor --distro <name>:

Prints a clean diagnostic table showing pass/warn/fail for: WSL2 Version, Target Distro State, VM GUID (wslinfo --vm-id), Systemd PID 1, /dev/dxg vGPU, Guest Binary Version Parity, and Detected Wayland Sessions.

Milestone 2: AF_HYPERV ↔ AF_VSOCK Zero-Network Control Plane
Goal: Prove sub-millisecond hypervisor socket communication with zero IP/firewall configuration.

Deliverables:

wsl-guest/src/transport.rs: Async AF_VSOCK listener on VMADDR_CID_ANY port 50001, speaking common::protocol framed ControlMessages (Hello → Welcome, Ping → Pong).

win-host/src/transport.rs: Win32 AF_HYPERV client resolving VmId dynamically via wslinfo --vm-id, constructing SOCKADDR_HV (Reserved = 0), and connecting to CONTROL_PORT.

Verification Command: wsldesk.exe ping --distro <name> --count 5 runs a 5-packet round-trip handshake over VMBus and reports min/avg/max latency in microseconds.

Milestone 3: Distro-Agnostic Wayland Session Isolator & Orchestrator
Goal: Launch any installed Wayland DE/WM headless on /dev/dxg without colliding with WSLg.

Deliverables:

wsl-guest/src/sessions.rs: Parse /usr/share/wayland-sessions/*.desktop (Name, Exec, DesktopNames), classify compositor family (wlroots, gnome, kde, cosmic, weston).

wsl-guest/src/env_isolate.rs:

Create isolated XDG_RUNTIME_DIR (/run/user/<uid>/wsldesk-rt, mode 0700).

Unshare mount namespace (CLONE_NEWNS) and mount a private tmpfs over /tmp/.X11-unix so XWayland inside the desktop session gets a clean :0 without touching WSLg.

Strip stale DISPLAY, WAYLAND_DISPLAY, XAUTHORITY while preserving /dev/dxg GPU access and D-Bus session bus.

Inject headless backend variables per compositor family (e.g., WLR_BACKENDS=headless, WLR_LIBINPUT_NO_DEVICES=1, or nested weston --backend=headless-backend.so fallback).

Verification Command: wsldesk.exe sessions --distro <name> lists detected desktops over AF_VSOCK; wsldesk.exe launch --distro <name> --session <id> --dry-run boots the headless compositor and verifies a new Wayland socket appears inside the isolated XDG_RUNTIME_DIR.

Milestone 4: Path B Native Windows Renderer (wgpu D3D12 + Wayland Capture)
Goal: Render the live Linux Wayland desktop in a native Direct3D 12 Windows window with full keyboard/mouse/resize/clipboard support.

Deliverables:

Guest Frame Capture (wsl-guest/src/capture.rs):

Pure-Rust wayland-client connecting to the isolated WAYLAND_DISPLAY.

Bind zwlr_screencopy_manager_v1 (or ext_image_copy_capture_v1), allocate memfd_create + wl_shm pools, capture frames at target refresh rate, compute tile diffs + lz4_flex compression, and stream over VIDEO_PORT (50002).

Bind zwp_virtual_keyboard_manager_v1 and zwlr_virtual_pointer_manager_v1 to inject host input events.

Host Native Viewport (win-host/src/renderer.rs):

Create winit window + wgpu (Backends::DX12) surface and fullscreen-triangle WGSL shader.

Background receiver thread decompresses incoming frames from VIDEO_PORT and uploads dirty regions via queue.write_texture().

Forward winit resize events as ControlMessage::Resize to dynamically reconfigure the headless Wayland output mode.

Map winit PhysicalKey → Linux evdev keycodes; add toggleable raw keyboard grab (e.g., Right Ctrl release hotkey) for Super/Alt+Tab; bridge Win32 clipboard ↔ Wayland wlr-data-control.

6. Reference Repositories for the Agent
When implementing tricky OS boundaries, consult the patterns from these open-source projects:

Hyper-V / VSOCK in Rust: ALinuxPerson/hv_sock, rust-vsock/vsock-rs, microsoft/WSL#5751, marchaesen/vcxsrv#80

Pure-Rust Wayland & Wire Serialization: mstoeckl/waypipe (Rust rewrite), Smithay/wayland-rs, Smithay/smithay

WSL2 System Distro & /dev/dxg: microsoft/wslg

Wayland Headless Capture / Input: any1/wayvnc, wlroots (wlr-screencopy-unstable-v1, virtual-keyboard-unstable-v1)

7. Agent Execution Rules (AGENTS.md Checklist)
Compile-Check Loop: After every multi-file edit, run cargo check --workspace and cargo clippy --workspace --all-targets and resolve all errors/warnings before reporting done.

No Silent Destructive Edits: Never touch .wslconfig or /etc/wsl.conf without creating a timestamped backup and printing a clear diff.

Strict Target Isolation: Never break Windows compilation with Linux-only imports or vice versa.

Pure-Rust Musl Discipline: Never add a C-linked crate to crates/wsl-guest without explicit approval.

Immediate First Task
Start Milestone 1 now:

Create the Cargo workspace (Cargo.toml, AGENTS.md, crates/common, crates/win-host, crates/wsl-guest) with proper target-gated dependencies.

Implement common::protocol framing + unit tests.

Implement win-host (wsl_cli.rs, config.rs, deployer.rs, main.rs with setup and doctor) and wsl-guest (--version and doctor-probe --json).

Run cargo test -p common and cargo check --workspace, then output exact build commands for both x86_64-pc-windows-msvc and x86_64-unknown-linux-musl plus how to run the first wsldesk.exe doctor test.
