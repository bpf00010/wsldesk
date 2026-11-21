#!/bin/sh
set -eu
cfg=${XDG_CONFIG_HOME:-"$HOME/.config"}
if [ "${1:-}" = starter ] && [ -e "$cfg/sway/config" ]; then exit 0; fi
mkdir -p "$cfg"/sway "$cfg"/waybar "$cfg"/foot "$cfg"/wofi "$cfg"/mako
write_config() {
    file=$cfg/$1
    if [ -e "$file" ]; then
        if [ "$replace" = yes ]; then
            cp -p "$file" "$file.wsldesk.$(date +%s).bak"
        else
            cat >/dev/null
            return
        fi
    fi
    cat >"$file"
}
replace=no
[ "${1:-}" != full ] || replace=yes
write_config sway/config <<'SWAY'
set $mod Mod4
set $term foot
set $menu wofi --show drun
font pango:DejaVu Sans 10
seat seat0 xcursor_theme Adwaita 24
gaps inner 8
gaps outer 4
default_border pixel 2
client.focused #cba6f7 #313244 #cdd6f4 #89b4fa #cba6f7
client.unfocused #45475a #1e1e2e #a6adc8 #45475a #45475a
client.urgent #f38ba8 #313244 #cdd6f4 #f38ba8 #f38ba8
exec_always swaybg -i ~/.config/sway/wallpaper.png -m fill
exec mako
exec waybar -c ~/.config/waybar/config-top.jsonc -s ~/.config/waybar/style-top.css
exec waybar -c ~/.config/waybar/config-dock.jsonc -s ~/.config/waybar/style-dock.css
bindsym $mod+Return exec $term
bindsym $mod+b exec sh -c 'if command -v wsldesk-chrome >/dev/null; then exec wsldesk-chrome; else exec xdg-open https://www.google.com; fi'
bindsym $mod+e exec thunar
bindsym $mod+d exec $menu
bindsym $mod+q kill
bindsym $mod+f fullscreen toggle
bindsym $mod+space floating toggle
bindsym $mod+Left focus left
bindsym $mod+Down focus down
bindsym $mod+Up focus up
bindsym $mod+Right focus right
bindsym $mod+Shift+e exit
SWAY
if [ "$replace" = yes ] || ! grep -q '^bindsym .*+1 workspace' "$cfg/sway/config"; then
    for n in 1 2 3 4 5 6 7 8 9; do
        printf 'bindsym $mod+%s workspace number %s\nbindsym $mod+Shift+%s move container to workspace number %s\n' "$n" "$n" "$n" "$n"
    done >>"$cfg/sway/config"
fi
write_config waybar/config-top.jsonc <<'JSON'
{
  "layer": "top", "position": "top", "margin": "8 12 0", "spacing": 8,
  "modules-left": ["sway/workspaces", "sway/window"],
  "modules-center": ["clock"],
  "modules-right": ["cpu", "memory", "disk", "pulseaudio", "custom/hint"],
  "sway/window": {"max-length": 36},
  "clock": {"format": "{:%a %d %b  %H:%M}", "tooltip-format": "<tt>{calendar}</tt>"},
  "cpu": {"format": "CPU {usage}%"},
  "memory": {"format": "RAM {percentage}%"},
  "disk": {"format": "Disk {percentage_used}%"},
  "pulseaudio": {"format": "Vol {volume}%", "format-muted": "Muted", "on-click": "pactl set-sink-mute @DEFAULT_SINK@ toggle"},
  "custom/hint": {"format": "R-Ctrl: Grab | R-Ctrl+F: Fullscreen"}
}
JSON
write_config waybar/style-top.css <<'CSS'
* { font-family: "DejaVu Sans"; font-size: 12px; border: none; }
window#waybar { background: rgba(30,30,46,0.94); color: #cdd6f4; border-radius: 12px; }
#workspaces button { color: #a6adc8; padding: 0 10px; }
#workspaces button.focused { background: #cba6f7; color: #181825; border-radius: 10px; }
#window, #clock, #cpu, #memory, #disk, #pulseaudio, #custom-hint { padding: 4px 8px; }
#clock { color: #cba6f7; font-weight: bold; }
#cpu, #memory, #disk { color: #a6e3a1; }
#custom-hint { color: #a6adc8; font-size: 10px; }
CSS
write_config waybar/config-dock.jsonc <<'JSON'
{
  "layer": "top", "position": "bottom", "width": 540, "height": 46,
  "margin-bottom": 12, "spacing": 6,
  "modules-center": ["custom/apps", "custom/chrome", "custom/terminal", "custom/files", "wlr/taskbar"],
  "custom/apps": {"format": "Apps", "on-click": "wofi --show drun"},
  "custom/chrome": {"format": "Chrome", "on-click": "if command -v wsldesk-chrome >/dev/null; then wsldesk-chrome; else xdg-open https://www.google.com; fi"},
  "custom/terminal": {"format": "Terminal", "on-click": "foot"},
  "custom/files": {"format": "Files", "on-click": "thunar"},
  "wlr/taskbar": {"format": "{icon}", "icon-size": 20, "on-click": "activate"}
}
JSON
write_config waybar/style-dock.css <<'CSS'
* { font-family: "DejaVu Sans"; font-size: 12px; border: none; }
window#waybar { background: rgba(30,30,46,0.9); color: #cdd6f4; border: 1px solid #45475a; border-radius: 23px; }
#custom-apps, #custom-chrome, #custom-terminal, #custom-files, #taskbar button { padding: 0 14px; border-radius: 18px; }
#custom-apps { color: #cba6f7; font-weight: bold; }
#taskbar button.active { background: #45475a; }
CSS
write_config foot/foot.ini <<'INI'
[main]
font=DejaVu Sans Mono:size=11
pad=12x12
[colors]
alpha=0.94
foreground=cdd6f4
background=1e1e2e
regular0=45475a
regular1=f38ba8
regular2=a6e3a1
regular3=f9e2af
regular4=89b4fa
regular5=cba6f7
regular6=94e2d5
regular7=bac2de
bright0=585b70
bright1=f38ba8
bright2=a6e3a1
bright3=f9e2af
bright4=89b4fa
bright5=cba6f7
bright6=94e2d5
bright7=a6adc8
INI
write_config wofi/style.css <<'CSS'
window { background-color: rgba(30,30,46,0.95); border: 2px solid #cba6f7; border-radius: 16px; color: #cdd6f4; }
#outer-box { padding: 12px; }
#input, #entry { padding: 8px; border-radius: 8px; }
#input, #entry:selected { background-color: #313244; color: #cdd6f4; }
CSS
write_config mako/config <<'INI'
font=DejaVu Sans 10
background-color=#1e1e2eee
text-color=#cdd6f4
border-color=#cba6f7
border-radius=12
padding=12
default-timeout=5000
INI
if [ ! -e "$cfg/sway/wallpaper.png" ]; then
    python3 - "$cfg/sway/wallpaper.png" <<'PYTHON'
import struct, sys, zlib
w, h = 1920, 1080
def chunk(tag, data):
    return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data))
rows = b"".join(b"\0" + bytes((24 + y*8//h, 18 + y*7//h, 38 + y*14//h))*w for y in range(h))
with open(sys.argv[1], "xb") as out:
    out.write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w,h,8,2,0,0,0)) +
              chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b""))
PYTHON
fi
