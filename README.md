# WSLDesk

WSLDesk opens a Linux Wayland desktop from WSL2 in a native Windows Direct3D 12 window. It is a Wayland-first alternative to Win-KeX. Input and video travel over Hyper-V sockets on VMBus, so the desktop bridge requires no NAT address, port forwarding, or firewall rule.

Unfortunately performance wise it is currently a mess in a heavy prerelease state. I wouldn't recommend actually fully using it just yet. I left agent configs and more extensive read me's around architecture in case you want to contribute.


## Install and launch

From PowerShell, use a release checkout with `dist/wsldesk.exe` or build it from source first. The installer copies one executable to your user profile and adds it to your user PATH.

```powershell
.\scripts\install.ps1
wsldesk doctor
wsldesk setup --install-desktop
wsldesk up
```

The installer also takes `-ReleaseUrl` with an HTTPS URL to the released executable. If an existing terminal cannot find `wsldesk`, open a new terminal. Desktop package installation can take several minutes. Setup preserves an existing Sway config. Use `--distro Ubuntu` to select a named distro. `up` prepares systemd and the graphics groups, deploys the embedded guest if its checksum has changed, and selects Sway when available. Try `wsldesk up --fullscreen --width 1920 --height 1080` for a full-screen session or `--no-audio` to disable audio passthrough.

## Desktop controls

| Control | Action |
| --- | --- |
| Click inside the window | Grab keyboard shortcuts |
| Right Ctrl or Ctrl+Alt+G | Toggle keyboard grab |
| Right Ctrl + F | Toggle Windows borderless fullscreen and grab on entry |
| Win + Enter | Open foot |
| Win + B | Open Chrome on the configured Ubuntu desktop |
| Win + D | Open the application launcher |
| Win + E | Open Thunar |
| Win + Q | Close the focused Linux window |
| Win + F / Win + Space | Toggle Linux fullscreen / floating |
| Win + 1–9 / Win + Shift + 1–9 | Switch workspaces / move the focused window |
| Clipboard | Text synchronizes between Windows and Wayland |
| Audio | Uses WSL audio passthrough when available. Click the volume widget to mute |
| Wallpaper | Replace `~/.config/sway/wallpaper.png` and reload Sway |

## Architecture

```mermaid
flowchart LR
  W["Windows · wsldesk.exe · DX12"] <-->|"VMBus · control 50001 · video 50002"| G["WSL2 · static wsldesk-guest"]
  G <-->|"Wayland screencopy, input and clipboard"| S["Sway · Linux apps"]
```

## Supported distros

Ubuntu on WSL2 is the release verification target. The starter installer also recognizes Fedora (DNF), Arch (pacman), and openSUSE (zypper). Package availability depends on the distro release and enabled repositories. The interactive capture path requires a compositor with the wlroots screencopy protocol. Chrome is installed separately from its official `.deb` on the configured Ubuntu desktop.

## Build from source

Use Rust with the Windows MSVC toolchain and install the guest target with `rustup target add x86_64-unknown-linux-musl`. Run `.\scripts\build-release.ps1` to build the static Linux guest first and embed it in `dist/wsldesk.exe`. The script also writes `dist/wsldesk.exe.sha256`.

## Troubleshooting

Run `wsldesk doctor --distro Ubuntu` for WSL version, systemd, GPU, guest binary, and session checks. If no desktop is detected, run `wsldesk setup --install-desktop`. If a session uses pixman, its compositor is rendering in software. If Win shortcuts reach Windows, click inside WSLDesk and check the grab indicator in the title.


##Roadmap


### High-Refresh & Frame Pacing (Breaking the 60 FPS Cap)
- **True 120–144 Hz Output & Capture Pacing:** Replace the hardcoded `60000` mHz mode in `zwlr_output_configuration_head_v1::set_custom_mode` with a configurable `--fps` / monitor-matched refresh rate (up to `144000` mHz+), and uncap the guest screencopy loop from its 60 FPS ceiling.
- **Persistent Reusable `wl_shm` Buffer Pool:** Eliminate per-frame `memfd_create` / `ftruncate` / `mmap` / `wl_shm_pool` creation and teardown in `capture_one_frame` by reusing a single mapped SHM pool (reallocating only on resize) to remove micro-stutters and kernel syscall overhead.
- **DX12 Low-Latency Presentation:** Switch the `wgpu` swapchain from VSync-bound `Fifo` to `Mailbox` / `AutoNoVsync` where supported, and decouple texture uploads (`queue.write_texture`) from `winit` event-loop scheduling to cut frame drops and pacing jitter.

### High-Resolution (1440p / 4K) Performance & Bandwidth
- **Frame Updates (`copy_with_damage`):** Upgrade from blind full-frame `zwlr_screencopy` polling to `copy_with_damage` with dirty-rectangle / tile diffing (leveraging `VideoFrameHeader.rect_count`), avoiding re-compressing and uploading 14–33 MB raw frames on every tick at 1440p/4K.
- **Idle Frame Suppression:** Skip shipping unchanged frames when the compositor reports zero damage instead of saturating CPU and VMBus with redundant full-screen LZ4 payloads.
- **Multi-Threaded Compression & Adaptive Quality:** Parallelize LZ4 tile compression across cores (and evaluate optional hardware/low-latency video codecs for motion-heavy workloads like browser video playback).
- **GPU-Accelerated Guest Compositing:** Investigate alternatives to the current `WLR_RENDERER=pixman` software compositing fallback (currently required because WSL2's `/dev/dri/renderD128` `dxg` DRM layer rejects `DRM_IOCTL_MODE_CREATE_DUMB` / GBM buffer allocation for headless outputs), freeing CPU headroom at high resolutions.
- **Fractional HiDPI & Crisp Scaling:** Improve `scale_milli` negotiation and viewport aspect/DPI synchronization so 1440p and 4K displays stay sharp without blurry upscaling or resize flicker.

### Input, Clipboard & Usability Bug Fixes
- **Fix Mouse Scroll Wheel:** Properly emit `zwlr_virtual_pointer_v1::axis_source` (`Wheel`), `axis_discrete`, calibrated `axis` deltas (both vertical and horizontal), and `frame()` so scrolling works reliably in Chrome, `foot`, and GTK/Qt apps.
- **Lower Input Latency:** Replace the 2 ms `select()` polling interval in `ControlPump` with event-driven signaling, fix remaining modifier/grab edge cases (`Alt+Tab` and focus transitions), and add configurable grab hotkeys.
- **Richer Clipboard Sync:** Extend Win32 ↔ `zwlr_data_control_v1` beyond `text/plain;charset=utf-8` to support images (`image/png`) and richer MIME types cleanly.
- **Session Persistence & Reattach (`wsldesk attach`):** Allow detaching and reattaching the Windows viewport without killing open Linux apps, alongside graceful recovery from transient VMBus hiccups.

### Broader Desktop Environment & Compositor Support
- **GNOME (Mutter) & KDE Plasma (KWin):** Implement capture and input injection through `xdg-desktop-portal` (ScreenCast + PipeWire + RemoteDesktop portals), since Mutter and KWin do not expose `zwlr_screencopy_manager_v1` or `zwlr_virtual_pointer_v1`.
- **Standardized Wayland Capture Protocols:** Add support for upstream `ext-image-copy-capture-v1` as compositors (including Weston, COSMIC, and newer wlroots/Smithay builds) adopt it.
- **First-Class Multi-Compositor Profiles:** Tested launch presets and starter configs for Hyprland, Labwc, Wayfire, River, and Weston in addition to Sway.
- **Multi-Distro Port Isolation:** Support dynamic or configurable `AF_VSOCK` ports (beyond hardcoded `50001`/`50002`) so sessions can run across multiple WSL2 distros concurrently without hitting `EADDRINUSE` in the shared WSL2 utility VM.
