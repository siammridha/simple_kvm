#!/bin/sh
# Run this on the Wyse 3040 itself, as root:
#
#   wget -qO- https://raw.githubusercontent.com/siammridha/simple_kvm/main/deploy/install.sh | sh
#
# Installs and checks what the binary needs to run, downloads the latest
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

# Packages, all installed every run (apk skips ones already present):
#   libgcc             - libgcc_s.so.1; the binary is dynamically linked
#                        against musl (-crt-static) and won't start without it
#   musl-utils         - ldd, used below to verify the downloaded binary
#   jq                 - parses the GitHub release JSON below
#   libva              - VAAPI runtime the H.264 encoder links against
#   libva-intel-driver - the classic `i965` driver; the newer
#                        `intel-media-driver`/iHD is Gen9+ and doesn't
#                        support this chip
#   libva-utils        - vainfo, for the H.264 encode check below
# The libva packages live in `community`, which isn't enabled by default;
# both repos are passed with `--repository` for this one command instead of
# editing /etc/apk/repositories (see docs/gpu-encoding-investigation.md).
ALPINE_VERSION="v$(cut -d. -f1,2 /etc/alpine-release)"
MAIN_REPO="http://dl-cdn.alpinelinux.org/alpine/$ALPINE_VERSION/main"
COMMUNITY_REPO="http://dl-cdn.alpinelinux.org/alpine/$ALPINE_VERSION/community"
PACKAGES="libgcc musl-utils jq libva libva-intel-driver libva-utils"

echo "Installing required packages ($PACKAGES)..."
# shellcheck disable=SC2086
if ! apk add --no-cache --repository "$MAIN_REPO" --repository "$COMMUNITY_REPO" $PACKAGES; then
	fail "could not install: $PACKAGES"
	exit 1
fi
ok "required packages installed"

# Kernel modules, loaded now and listed in /etc/modules so they also load on
# every boot:
#   i915     - Intel GPU; provides /dev/dri/renderD* for the encoder
#   uvcvideo - USB capture card (/dev/video*)
#   ch341    - CH340 USB-serial chip the CH9329 sits behind (/dev/ttyUSB*)
for MODULE in i915 uvcvideo ch341; do
	if ! modprobe "$MODULE"; then
		fail "could not load kernel module $MODULE - is this the stock Alpine kernel (linux-lts)?"
		exit 1
	fi
	grep -qx "$MODULE" /etc/modules 2>/dev/null || echo "$MODULE" >> /etc/modules
	ok "kernel module $MODULE loaded (and set to load at boot)"
done

if ! ls /dev/dri/renderD* >/dev/null 2>&1; then
	fail "no /dev/dri/renderD* device - no GPU render node for the H.264 encoder to use"
	exit 1
fi
ok "GPU render device present ($(ls /dev/dri/renderD* | head -1))"

VAINFO_OUT=$(vainfo 2>&1 || true)
if ! echo "$VAINFO_OUT" | grep -q "i965" \
	|| ! echo "$VAINFO_OUT" | grep -q "VAProfileH264ConstrainedBaseline.*VAEntrypointEncSlice"; then
	fail "vainfo didn't report the i965 driver with H.264 encode support - the encoder can't start"
	echo "$VAINFO_OUT" | sed 's/^/         /' >&2
	exit 1
fi
ok "driver reports H.264 encode support (vainfo)"

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

MISSING_LIBS=$(ldd /usr/local/bin/simple_kvm.new 2>&1 | grep -i "not found" || true)
if [ -n "$MISSING_LIBS" ]; then
	rm -f /usr/local/bin/simple_kvm.new
	fail "the binary needs shared libraries that aren't installed:"
	echo "$MISSING_LIBS" | sed 's/^/         /' >&2
	exit 1
fi
ok "all shared libraries the binary needs are present"

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
