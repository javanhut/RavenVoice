#!/usr/bin/env bash
# Install an already-built RavenVoice for the current user. It is an app like
# any other: it runs when opened from the launcher and stops when closed.
#
#   scripts/install.sh [prefix]            install (default prefix: ~/.local)
#   scripts/install.sh --uninstall [prefix]
#
# RAVENVOICE_AUTOSTART=yes also starts it with every session: on Raven, a
# supervised `raven-init --user` service; elsewhere an XDG autostart entry.
# Without it, either left by an earlier install is removed.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
uninstall=false
if [[ ${1:-} == --uninstall ]]; then
    uninstall=true
    shift
fi
prefix="${1:-$HOME/.local}"
config="${XDG_CONFIG_HOME:-$HOME/.config}"
bin="$prefix/bin/ravenvoice"
desktop="$prefix/share/applications/org.raven.RavenVoice.desktop"
raven_service="$config/raven/services/ravenvoice.toml"
xdg_autostart="$config/autostart/org.raven.RavenVoice.desktop"

on_raven() { [[ -d /usr/share/raven/user-services ]] && command -v raven-rc >/dev/null 2>&1; }

if $uninstall; then
    if [[ -e $raven_service ]]; then
        raven-rc --user stop ravenvoice >/dev/null 2>&1 || true
        rm -f "$raven_service"
        raven-rc --user reload >/dev/null 2>&1 || true
    fi
    "$bin" quit >/dev/null 2>&1 || true
    rm -f "$bin" "$desktop" "$xdg_autostart"
    update-desktop-database "$prefix/share/applications" >/dev/null 2>&1 || true
    echo "Removed RavenVoice from $prefix."
    echo "Settings (~/.config/ravenvoice) and speech models (~/.local/share/ravenvoice) are left in place."
    exit 0
fi

install -Dm755 "$here/target/release/ravenvoice" "$bin"
install -Dm644 "$here/data/org.raven.RavenVoice.desktop" "$desktop"
# The entry ships with a bare `Exec=ravenvoice`; point it at where it landed.
sed -i "s|^Exec=ravenvoice\$|Exec=$bin|" "$desktop"
update-desktop-database "$prefix/share/applications" >/dev/null 2>&1 || true
echo "Installed $bin"

if [[ ${RAVENVOICE_AUTOSTART:-no} != yes ]]; then
    # An earlier install started it with the session and restarted it
    # whenever it was closed; take that away.
    if [[ -e $raven_service ]]; then
        raven-rc --user stop ravenvoice >/dev/null 2>&1 || true
        rm -f "$raven_service"
        raven-rc --user reload >/dev/null 2>&1 || true
        echo "Removed the raven-init user service; open RavenVoice from the launcher."
    fi
    if [[ -e $xdg_autostart ]]; then
        rm -f "$xdg_autostart"
        echo "Removed $xdg_autostart."
    fi
else
    if on_raven; then
        mkdir -p "$(dirname "$raven_service")"
        sed "s|@BINDIR@|$prefix/bin|" "$here/data/ravenvoice.service.toml" >"$raven_service"
        # A copy started by hand would hold the control socket; let the
        # supervised one take over.
        "$bin" quit >/dev/null 2>&1 || true
        if raven-rc --user reload >/dev/null 2>&1; then
            raven-rc --user restart ravenvoice >/dev/null 2>&1 \
                || raven-rc --user start ravenvoice >/dev/null 2>&1 || true
            echo "Registered the raven-init user service (logs: raven-rc --user logs ravenvoice)"
        else
            echo "Wrote $raven_service; it starts at your next login."
        fi
    else
        install -Dm644 "$desktop" "$xdg_autostart"
        echo "Added $xdg_autostart; RavenVoice starts at your next login."
    fi
fi
