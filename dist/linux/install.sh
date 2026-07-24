#!/usr/bin/env bash
# Install or remove the accesskit_remoted user service inside a WSL distro.
#
#   install.sh [install]     place the binary + systemd --user unit, enable it
#   install.sh --uninstall   stop and remove them
#
# Distro-generic: relies only on systemd (as PID 1) and loginctl. The daemon
# binary is taken from next to this script (./accesskit_remoted or
# ./<uname -m>/accesskit_remoted).
set -euo pipefail

service=accesskit-remoted
binname=accesskit_remoted
bindir="$HOME/.local/bin"
unitdir="$HOME/.config/systemd/user"
binpath="$bindir/$binname"
unitpath="$unitdir/$service.service"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

die() { echo "install.sh: $*" >&2; exit 1; }

do_uninstall() {
  systemctl --user disable --now "$service.service" 2>/dev/null || true
  rm -f "$unitpath" "$binpath"
  systemctl --user daemon-reload 2>/dev/null || true
  echo "install.sh: removed $service"
}

do_install() {
  [ -d /run/systemd/system ] \
    || die "systemd is not PID 1; set 'systemd=true' in /etc/wsl.conf and run 'wsl --shutdown'"
  command -v systemctl >/dev/null || die "systemctl not found"

  local src="$here/$binname"
  [ -f "$src" ] || src="$here/$(uname -m)/$binname"
  [ -f "$src" ] || die "daemon binary '$binname' not found next to install.sh"

  mkdir -p "$bindir" "$unitdir"
  install -m 0755 "$src" "$binpath"
  install -m 0644 "$here/$service.service" "$unitpath"

  # Keep the user manager (and the service) alive without an interactive login.
  loginctl enable-linger "$USER" 2>/dev/null || true

  systemctl --user daemon-reload
  systemctl --user enable --now "$service.service"
  echo "install.sh: installed and started $service ($binpath)"
}

case "${1:-install}" in
  install | "") do_install ;;
  --uninstall | uninstall) do_uninstall ;;
  *) die "usage: install.sh [install|--uninstall]" ;;
esac
