# AGENTS.md — WSLDesk Agent Guardrails

Strict rules for any agent working on this repository. The full specification lives in `Plan.Md`; read it before changing architecture.

## Milestone Discipline
- Implement ONE milestone at a time (see `Plan.Md` §5). Never write Milestone 4 renderer code while implementing Milestone 1.
- Every milestone must compile cleanly and pass its verification command before advancing.

## Compile-Check Loop (run after every multi-file edit)
```powershell
cargo check --workspace
cargo clippy --workspace --all-targets
cargo test -p common
# Cross-check the Linux guest from Windows (no linker needed for check):
rustup target add x86_64-unknown-linux-musl
cargo check -p wsl-guest --target x86_64-unknown-linux-musl
```
Resolve all errors/warnings before reporting done.

## No Silent Destructive Edits
- Never touch `%USERPROFILE%\.wslconfig` or `/etc/wsl.conf` without creating a timestamped backup (`.wslconfig.wsldesk.<unix_ts>.bak`, `/etc/wsl.conf.wsldesk.<ts>.bak`) and printing the exact line diff.

## Strict Target Isolation
- Windows-only crates go under `[target.'cfg(windows)'.dependencies]` in `crates/win-host/Cargo.toml`.
- Linux-only crates go under `[target.'cfg(target_os = "linux")'.dependencies]` in `crates/wsl-guest/Cargo.toml`.
- Gate real implementations behind `#[cfg(windows)]` / `#[cfg(target_os = "linux")]`; provide compile-time stubs for the opposite OS so `cargo check --workspace` stays green on both sides.

## Pure-Rust Musl Discipline
- `crates/wsl-guest` builds as a 100% statically-linked `x86_64-unknown-linux-musl` binary. NEVER add a C-linked crate (glibc/dlopen-dependent: libwayland-client, libpipewire, libdbus, OpenSSL, …) without explicit approval.
- `wayland-client`/`wayland-protocols` must keep default features (never enable the `system` feature).
- PipeWire is Tier 2 only (GNOME/KDE) — see Plan.Md §2.

## Wire Protocol Rules
- postcard/serde is ONLY for small control messages and fixed headers. NEVER serialize raw pixel buffers through serde; stream pixel bytes raw on the VIDEO socket.
- Control plane: port 50001 (`b"WSLD"` frames), video plane: port 50002 (`b"WFRM"` frames).

## Transport Direction (CRITICAL, Milestone 2)
- The Linux guest is the AF_VSOCK **listener** (`VMADDR_CID_ANY`); the Windows host is the AF_HYPERV **client** using the VM GUID from `wsl.exe -d <distro> -- wslinfo --vm-id`. Reversing the direction causes WSAEACCES (10013) on non-elevated shells.

## wsl.exe Handling
- Always set `WSL_UTF8=1` on `wsl.exe` invocations AND keep the defensive `decode_wsl_output` UTF-16LE fallback.
- Zero-network rule: never use NAT IPs, localhost forwarding, or firewall rules; Hyper-V sockets only.
