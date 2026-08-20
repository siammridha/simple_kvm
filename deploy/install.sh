#!/bin/sh
# Run this on the Wyse 3040 itself, as root:
#
#   wget -qO- https://raw.githubusercontent.com/siammridha/simple_kvm/main/deploy/install.sh | sh
#
# Downloads the latest release binary from GitHub, installs it as an OpenRC
# boot service, and starts it. No other files need to be copied to the
# device first.
set -eu

REPO="siammridha/simple_kvm"

if [ "$(id -u)" -ne 0 ]; then
	echo "Run this as root." >&2
	exit 1
fi

apk add --no-cache jq

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

# Starting the capture card right as it finishes USB enumeration at boot
# reliably hard-crashes this specific Wyse 3040 (confirmed by testing:
# starting this service at boot crashes it every time, starting the exact
# same way once the system's been up a while never does). Giving the USB
# subsystem time to settle before opening the devices avoids it.
start_pre() {
	sleep 30
}
EOF
chmod 755 /etc/init.d/simple_kvm

rc-update add simple_kvm default
rc-service simple_kvm restart

echo "simple_kvm installed and running. Status:"
rc-service simple_kvm status
