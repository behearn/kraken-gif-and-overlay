#!/bin/sh
set -eu
cd "$(CDPATH= cd -- "$(dirname "$0")" && pwd)"

bin="$PWD/target/release/kraken-gif-and-overlay"
if [ ! -x "$bin" ]; then
    echo "Run ./build.sh first." >&2
    exit 1
fi

if [ "$(id -u)" -ne 0 ]; then
    exec sudo -- "$PWD/install.sh"
fi

user=${SUDO_USER:-}
if [ -z "$user" ] || [ "$user" = root ]; then
    echo "Run ./install.sh as the user who owns the GIF config, not from a root shell." >&2
    exit 1
fi

home=$(getent passwd "$user" | cut -d: -f6)
group=$(id -gn "$user")
if [ -z "$home" ] || [ ! -d "$home" ]; then
    echo "Could not find a home directory for $user." >&2
    exit 1
fi

install -Dm755 "$bin" /usr/local/bin/kraken-gif-and-overlay
sed -e "s|@USER@|$user|g" -e "s|@GROUP@|$group|g" -e "s|@HOME@|$home|g" \
    "$PWD/kraken-gif-and-overlay.service" > /etc/systemd/system/kraken-gif-and-overlay.service

cat > /etc/udev/rules.d/70-kraken-gif-and-overlay.rules << EOF
SUBSYSTEM=="usb", ATTR{idVendor}=="1e71", ATTR{idProduct}=="300c", MODE="0660", GROUP="$group"
SUBSYSTEM=="hidraw", ATTRS{idVendor}=="1e71", ATTRS{idProduct}=="300c", MODE="0660", GROUP="$group"
EOF
udevadm control --reload-rules
udevadm trigger --subsystem-match=usb --attr-match=idVendor=1e71 --attr-match=idProduct=300c
udevadm trigger --subsystem-match=hidraw

data="$home/.local/share/kraken-gif-and-overlay"
install -d -o "$user" -g "$group" "$data"

systemctl daemon-reload
systemctl enable kraken-gif-and-overlay.service

if [ -f "$data/config" ]; then
    systemctl restart kraken-gif-and-overlay.service
    echo "Started kraken-gif-and-overlay for $user."
else
    echo "Enabled kraken-gif-and-overlay, but did not start it."
    echo "Save a config first:"
    echo "  /usr/local/bin/kraken-gif-and-overlay --gif <path> --save-config"
fi

echo "Logs: journalctl -u kraken-gif-and-overlay -e"
