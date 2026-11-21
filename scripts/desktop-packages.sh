#!/bin/sh
set -eu
base='sway swaybg foot waybar wofi wl-clipboard thunar python3'
if command -v apt-get >/dev/null; then
    apt-get update
    apt-get install -y $base mako-notifier xwayland fonts-dejavu fonts-font-awesome fonts-noto-color-emoji adwaita-icon-theme pulseaudio-utils
elif command -v dnf >/dev/null; then
    dnf install -y $base mako xorg-x11-server-Xwayland dejavu-sans-fonts dejavu-sans-mono-fonts google-noto-emoji-color-fonts adwaita-icon-theme pulseaudio-utils
elif command -v pacman >/dev/null; then
    pacman -Syu --needed --noconfirm $base mako xorg-xwayland ttf-dejavu noto-fonts-emoji adwaita-icon-theme libpulse
elif command -v zypper >/dev/null; then
    zypper --non-interactive install $base mako xwayland dejavu-fonts noto-coloremoji-fonts adwaita-icon-theme pulseaudio-utils
else
    printf '%s\n' 'No supported package manager. Install Sway and its desktop tools manually.' >&2
    exit 1
fi
