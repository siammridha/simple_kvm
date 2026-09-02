#!/bin/sh
# Run this on the Wyse 3040 itself, as root:
#
#   wget -qO- https://raw.githubusercontent.com/siammridha/simple_kvm/main/deploy/install.sh | sh
#
# Checks the device has what the binary needs to run, downloads the latest
# release binary from GitHub, installs it as an OpenRC boot service, and
# starts it. No other files need to be copied to the device first.
set -eu

REPO="siammridha/simple_kvm"

# Printed unconditionally (no terminal check) - same as the colored startup
# banner src/main.rs prints (log_startup_banner), which also doesn't check.
GREEN='\033[32m'; YELLOW='\033[33m'; RED='\033[31m'; RESET='\033[0m'

ok()   { printf '  %b[ok]%b   %s\n' "$GREEN" "$RESET" "$1"; }
warn() { printf '  %b[warn]%b %s\n' "$YELLOW" "$RESET" "$1"; }
fail() { printf '  %b[FAIL]%b %s\n' "$RED" "$RESET" "$1" >&2; }

echo "Checking the environment..."

# The GPU H.264 encoder (src/capture/h264.rs) has no CPU fallback - if VAAPI
# setup fails, the binary itself refuses to start rather than silently
# falling back to software encoding. So everything below that the encoder
# needs is checked (and where possible, installed) up front, before we even
# download the binary - a clear failure here beats a confusing one from the
# service log after install.

if [ "$(id -u)" -ne 0 ]; then
	fail "not running as root - re-run as root (or with sudo)"
	exit 1
fi
ok "running as root"

ARCH="$(uname -m)"
if [ "$ARCH" != "x86_64" ]; then
	fail "architecture is $ARCH, but the release binary is built for x86_64 only"
	exit 1
fi
ok "architecture is x86_64"

if ! command -v apk >/dev/null 2>&1; then
	fail "apk not found - this installer is written for Alpine Linux"
	exit 1
fi
ok "Alpine Linux (apk found)"

if ! ls /dev/dri/renderD* >/dev/null 2>&1; then
	fail "no /dev/dri/renderD* device - no GPU render node for the H.264 encoder to use"
	exit 1
fi
ok "GPU render device present ($(ls /dev/dri/renderD* | head -1))"

apk add --no-cache jq >/dev/null

# libva (the VAAPI runtime) and libva-intel-driver (the classic `i965`
# driver this GPU generation needs - the newer `intel-media-driver`/iHD
# targets Gen9+ and doesn't support this chip) both live in Alpine's
# `community` repo, which isn't enabled by default. Installed with
# `--repository` for this one command, same as documented in
# docs/gpu-encoding-investigation.md, rather than editing
# /etc/apk/repositories - these are the only community packages needed.
COMMUNITY_REPO="http://dl-cdn.alpinelinux.org/alpine/v$(cut -d. -f1,2 /etc/alpine-release)/community"

need_install() { ! apk info -e "$1" >/dev/null 2>&1; }

if need_install libva || need_install libva-intel-driver; then
	echo "Installing the VAAPI runtime and Intel i965 driver from the community repo..."
	if ! apk add --no-cache --repository "$COMMUNITY_REPO" libva libva-intel-driver; then
		fail "could not install libva / libva-intel-driver from $COMMUNITY_REPO"
		echo "The H.264 encoder needs these to run - install them manually and re-run this script." >&2
		exit 1
	fi
fi
ok "VAAPI runtime (libva) installed"
ok "Intel i965 VAAPI driver (libva-intel-driver) installed"

if command -v vainfo >/dev/null 2>&1; then
	VAINFO_OUT=$(vainfo 2>&1 || true)
	if echo "$VAINFO_OUT" | grep -q "i965" \
		&& echo "$VAINFO_OUT" | grep -q "VAProfileH264ConstrainedBaseline.*VAEntrypointEncSlice"; then
		ok "driver reports H.264 encode support (vainfo)"
	else
		warn "vainfo didn't report the i965 driver with H.264 encode support - the encoder may fail to start"
		echo "$VAINFO_OUT" | sed 's/^/         /' >&2
	fi
else
	warn "vainfo not installed (apk add libva-utils) - skipping the deeper driver check"
fi

# The capture card and CH9329 are both hot-pluggable and the app runs fine
# without either already plugged in (see README.md's "The server runs fine
# with no capture card or CH9329 attached") - so these are informational
# only, not install blockers.
if ls /dev/video* >/dev/null 2>&1; then
	ok "capture card detected ($(ls /dev/video* | tr '\n' ' '))"
else
	warn "no /dev/video* device - capture card not plugged in yet (fine, the app will start without it)"
fi

if ls /dev/ttyUSB* >/dev/null 2>&1; then
	ok "CH9329 serial adapter detected ($(ls /dev/ttyUSB* | tr '\n' ' '))"
else
	warn "no /dev/ttyUSB* device - CH9329 not plugged in yet (fine, the app will start without it)"
fi

echo "Environment check passed."

echo "Looking up the latest release of $REPO..."
DOWNLOAD_URL=$(wget -qO- "https://api.github.com/repos/$REPO/releases/latest" \
	| jq -r '.assets[] | select(.name == "simple_kvm") | .browser_download_url')

if [ -z "$DOWNLOAD_URL" ]; then
	echo "Could not find a 'simple_kvm' asset in the latest release of $REPO." >&2
	exit 1
fi

echo "Downloading $DOWNLOAD_URL..."
# Download to a temp file and rename it into place, rather than overwriting
# /usr/local/bin/simple_kvm directly: if the service is already running from
# a previous install, writing straight to that path fails with "Text file
# busy". A rename works because it swaps the directory entry instead of
# touching the file the running process has open.
wget -qO /usr/local/bin/simple_kvm.new "$DOWNLOAD_URL"
chmod 755 /usr/local/bin/simple_kvm.new
mv /usr/local/bin/simple_kvm.new /usr/local/bin/simple_kvm

cat > /etc/init.d/simple_kvm <<'EOF'
#!/sbin/openrc-run

name="simple_kvm"
description="Simple KVM"

command="/usr/local/bin/simple_kvm"
command_background="yes"
pidfile="/run/${RC_SVCNAME}.pid"
output_log="/var/log/simple_kvm.log"
error_log="/var/log/simple_kvm.log"

depend() {
	need net
}
EOF
chmod 755 /etc/init.d/simple_kvm

# OpenRC sources conf.d automatically before the init script runs - this is
# what keeps an installed service at plain info-level logging (no per-frame
# or per-packet debug lines) regardless of what main.rs's own compiled-in
# default is. test-on-device.sh writes this same file with a debug-level
# RUST_LOG for its own test deploys.
echo 'export RUST_LOG=info' > /etc/conf.d/simple_kvm

rc-update add simple_kvm default
rc-service simple_kvm restart

echo "simple_kvm installed and running. Status:"
rc-service simple_kvm status
