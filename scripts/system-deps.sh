#!/usr/bin/env bash
# Make sure everything RavenVoice needs from the system is in place, and
# install whatever is missing. Every imlazy command that builds or runs
# RavenVoice depends on this, so a fresh checkout needs nothing done by hand.
#
# On Raven it installs through `rvn`; elsewhere through the distribution's own
# package manager (pacman, apt, dnf, zypper, xbps, apk), with each package
# translated to that distribution's name for it.
#
# Checked by what the build and the program actually look for -- a pkg-config
# module, a command on PATH, write access to /dev/uinput -- rather than by
# package name, so it costs a few milliseconds when all is well and also
# counts things installed some other way.
#
#   scripts/system-deps.sh           install what is missing
#   scripts/system-deps.sh --check   only report; exit 1 if anything is missing
set -euo pipefail

check_only=false
[[ ${1:-} == --check ]] && check_only=true
here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# ---------------------------------------------------------------- packages --

# Which family of package names this machine uses, and how to install.
family=""
installer=()
if command -v rvn >/dev/null 2>&1; then
    # Official repositories only: everything is there, and nothing here
    # should quietly build something from the AUR.
    family=arch; installer=(rvn install --repo-only --yes)
elif command -v pacman >/dev/null 2>&1; then
    family=arch; installer=(pacman -S --needed --noconfirm)
elif command -v apt-get >/dev/null 2>&1; then
    family=debian; installer=(apt-get install -y)
elif command -v dnf >/dev/null 2>&1; then
    family=fedora; installer=(dnf install -y)
elif command -v zypper >/dev/null 2>&1; then
    family=suse; installer=(zypper --non-interactive install)
elif command -v xbps-install >/dev/null 2>&1; then
    family=void; installer=(xbps-install -Sy)
elif command -v apk >/dev/null 2>&1; then
    family=alpine; installer=(apk add)
fi

# probe | arch | debian | fedora | suse | void | alpine
requirements=(
    "cmd:cargo|rust|cargo|cargo|cargo|cargo|cargo"
    "cmd:c++|gcc|build-essential|gcc-c++|gcc-c++|base-devel|build-base"
    "cmake|cmake|cmake|cmake|cmake|cmake|cmake"
    "cmd:pkg-config|pkgconf|pkg-config|pkgconf-pkg-config|pkg-config|pkg-config|pkgconf"
    "pc:gtk4 >= 4.14|gtk4|libgtk-4-dev|gtk4-devel|gtk4-devel|gtk4-devel|gtk4.0-dev"
    "pc:gtk4-layer-shell-0|gtk4-layer-shell|libgtk4-layer-shell-dev|gtk4-layer-shell-devel|gtk4-layer-shell-devel|gtk4-layer-shell-devel|gtk4-layer-shell-dev"
    "pc:alsa|alsa-lib|libasound2-dev|alsa-lib-devel|alsa-devel|alsa-lib-devel|alsa-lib-dev"
    "cmd:espeak-ng|espeak-ng|espeak-ng|espeak-ng|espeak-ng|espeak-ng|espeak-ng"
)

present() {
    local probe=$1
    case $probe in
        cmd:*) command -v "${probe#cmd:}" >/dev/null 2>&1 ;;
        # Without pkg-config nothing can be checked; pkgconf is listed before
        # the libraries, so it gets installed in the same run.
        pc:*) command -v pkg-config >/dev/null 2>&1 && pkg-config --exists "${probe#pc:}" ;;
        # .cargo/config.toml points the whisper.cpp build at /usr/bin/cmake.
        cmake) /usr/bin/cmake --version >/dev/null 2>&1 ;;
    esac
}

package_for() {
    local -a cols
    IFS='|' read -ra cols <<<"$1"
    case $family in
        arch) echo "${cols[1]}" ;;
        debian) echo "${cols[2]}" ;;
        fedora) echo "${cols[3]}" ;;
        suse) echo "${cols[4]}" ;;
        void) echo "${cols[5]}" ;;
        alpine) echo "${cols[6]}" ;;
        *) echo "${cols[1]}" ;;
    esac
}

missing_packages() {
    for requirement in "${requirements[@]}"; do
        present "${requirement%%|*}" || package_for "$requirement"
    done | sort -u
}

# ---------------------------------------------------------- virtual keyboard --

# Typing into other apps goes through /dev/uinput, which is root-only by
# default. The rule hands it to the `input` group, the same group that already
# reads every keyboard (and that the global shortcuts rely on).
uinput_ready() { [[ -w /dev/uinput ]]; }
in_input_group() { id -nG "${SUDO_USER:-$USER}" | tr ' ' '\n' | grep -qx input; }

# ------------------------------------------------------------------- report --

mapfile -t missing < <(missing_packages)
needs_uinput=false
uinput_ready || needs_uinput=true
needs_group=false
in_input_group || needs_group=true

if ((${#missing[@]} == 0)) && ! $needs_uinput && ! $needs_group; then
    exit 0
fi

((${#missing[@]} > 0)) && echo "RavenVoice needs these packages: ${missing[*]}"
$needs_uinput && echo "RavenVoice needs permission to use /dev/uinput (to type into other apps)"
$needs_group && echo "RavenVoice needs you in the 'input' group (shortcuts and typing)"
if $check_only; then
    exit 1
fi

sudo=()
((EUID == 0)) || sudo=(sudo)

# ------------------------------------------------------------------ install --

if ((${#missing[@]} > 0)); then
    if [[ -z $family ]]; then
        echo "No supported package manager found; install these yourself: ${missing[*]}" >&2
        exit 1
    fi
    [[ $family == debian ]] && "${sudo[@]}" apt-get update -qq
    "${sudo[@]}" "${installer[@]}" "${missing[@]}"
fi

if $needs_uinput; then
    echo "==> Granting the input group access to /dev/uinput"
    if [[ ! -e /dev/uinput ]]; then
        "${sudo[@]}" modprobe uinput
        echo uinput | "${sudo[@]}" tee /etc/modules-load.d/ravenvoice.conf >/dev/null
    fi
    "${sudo[@]}" install -Dm644 "$here/data/70-ravenvoice-uinput.rules" \
        /etc/udev/rules.d/70-ravenvoice-uinput.rules
    "${sudo[@]}" udevadm control --reload 2>/dev/null || true
    "${sudo[@]}" udevadm trigger --subsystem-match=misc --sysname-match=uinput 2>/dev/null || true
    # Apply now as well, whatever the udev implementation does with the trigger.
    "${sudo[@]}" chgrp input /dev/uinput
    "${sudo[@]}" chmod 0660 /dev/uinput
fi

if $needs_group; then
    user="${SUDO_USER:-$USER}"
    echo "==> Adding $user to the input group"
    "${sudo[@]}" usermod -aG input "$user"
    echo "    Log out and back in once for the new group to take effect."
fi

# ------------------------------------------------------------------- verify --

# Say so if the install did not provide what was missing, rather than letting
# the build fail with a pkg-config error further down.
mapfile -t still < <(missing_packages)
if ((${#still[@]} > 0)); then
    echo "Still missing after install: ${still[*]}" >&2
    exit 1
fi

if command -v rustc >/dev/null 2>&1; then
    minor=$(rustc --version | sed -E 's/^rustc 1\.([0-9]+).*/\1/')
    if [[ $minor =~ ^[0-9]+$ ]] && ((minor < 85)); then
        echo "rustc is too old for RavenVoice (edition 2024 needs 1.85+)." >&2
        echo "Install a current toolchain from https://rustup.rs" >&2
        exit 1
    fi
fi
echo "System dependencies installed."
