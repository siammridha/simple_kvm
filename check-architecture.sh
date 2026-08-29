#!/bin/sh
# Runs the enforcement checks written down in docs/ARCHITECTURE.md section 8,
# so the module boundaries can't quietly rot. Run from anywhere:
#
#   ./check-architecture.sh
#
# Exits 0 when every mechanical check passes, 1 otherwise. A failure names the
# file, the line and the invariant that broke.
#
# Only three of section 8's five checks are decidable by pattern matching.
# Checks 4 and 5 are partly a judgement call, so this script proves the part
# that is mechanical and says so in its summary. A green run is NOT proof that
# all five hold - read the summary it prints.
set -u

cd "$(dirname "$0")" || exit 1

VIOLATIONS=$(mktemp)
trap 'rm -f "$VIOLATIONS"' EXIT

# Every module in section 4's graph. `main.rs` is not one of them: a
# composition root has to name concrete types to construct them, and section 7
# says outright that naming a type is not a dependency edge, so its imports are
# never measured against the table.
MODULES="capture device hid rtc web"

# Section 4's allowed-edges table, one line per module. `device` is a leaf and
# imports no sibling.
allowed_edges_for() {
	case "$1" in
	capture) echo "device" ;;
	hid) echo "device" ;;
	rtc) echo "capture hid device" ;;
	web) echo "rtc" ;;
	device) echo "" ;;
	*) echo "" ;;
	esac
}

# Section 7: `main.rs` initialises tracing and prints the startup banner. That
# is process-level observability, not domain logic, so these two are the only
# functions it may define besides `main` itself.
MAIN_ALLOWED_FNS="main init_logging log_startup_banner"

record() {
	printf '%s\n' "$1" >>"$VIOLATIONS"
}

# Everything under src/ that is not the device module, in both the directory
# and the older single-file spelling of it (section 8's preamble).
outside_device() {
	grep -v -e '^src/device/' -e '^src/device\.rs:'
}

module_of() {
	m=${1#src/}
	m=${m%%/*}
	printf '%s\n' "${m%.rs}"
}

# --- Check 1: path secrecy (I3) ---------------------------------------------
# No OS device path and no device *_PATH env read outside the device module.
check_path_secrecy() {
	grep -rn --include='*.rs' -e '/dev/' src 2>/dev/null | outside_device |
		while IFS= read -r hit; do
			record "check 1 (path secrecy, I3): ${hit%%:*}:$(printf '%s' "$hit" | cut -d: -f2): device path outside the device module - only device/ may name an OS path
    ${hit#*:*:}"
		done

	grep -rn --include='*.rs' -e '_PATH' src 2>/dev/null | outside_device |
		while IFS= read -r hit; do
			record "check 1 (path secrecy, I3): ${hit%%:*}:$(printf '%s' "$hit" | cut -d: -f2): device *_PATH env read outside the device module - each Device reads its own path
    ${hit#*:*:}"
		done
}

# --- Check 2: dependency edges (I5) -----------------------------------------
# Every cross-module `crate::` reference has to appear in section 4's table.
check_dependency_edges() {
	for file in $(find src -name '*.rs' | sort); do
		[ "$file" = "src/main.rs" ] && continue
		mod=$(module_of "$file")
		allowed=$(allowed_edges_for "$mod")

		grep -n 'crate::' "$file" 2>/dev/null |
			while IFS= read -r hit; do
				lineno=${hit%%:*}
				text=${hit#*:}

				for target in $(printf '%s' "$text" | grep -o 'crate::[a-z_][a-z_0-9]*' | sed 's/crate:://' | sort -u); do
					[ "$target" = "$mod" ] && continue

					case " $MODULES " in
					*" $target "*) ;;
					*) continue ;; # not a module of the graph, e.g. a helper path
					esac

					case " $allowed " in
					*" $target "*) ;;
					*)
						record "check 2 (dependency edges, I5): $file:$lineno: $mod -> $target is not in the allowed-edges table (allowed for $mod: ${allowed:-none})
    $text"
						;;
					esac
				done
			done
	done
}

# --- Check 3: config locality (I2) ------------------------------------------
# Each module reads its own config; the composition root holds none.
check_config_locality() {
	grep -n -e 'env::var' -e '/dev/' src/main.rs 2>/dev/null |
		while IFS= read -r hit; do
			record "check 3 (config locality, I2): src/main.rs:${hit%%:*}: the composition root reads config - every module loads its own
    ${hit#*:}"
		done
}

# --- Check 4: composition root (I1), mechanical part ------------------------
# main.rs declares, builds, wires and starts. It defines no type and no
# function beyond main plus section 7's logging setup and banner.
check_composition_root() {
	grep -n '^[[:space:]]*\(pub \)\?\(async \)\?fn [a-z_]' src/main.rs 2>/dev/null |
		while IFS= read -r hit; do
			lineno=${hit%%:*}
			text=${hit#*:}
			name=$(printf '%s' "$text" | sed 's/.*fn \([a-z_0-9]*\).*/\1/')
			case " $MAIN_ALLOWED_FNS " in
			*" $name "*) ;;
			*)
				record "check 4 (composition root, I1): src/main.rs:$lineno: main.rs defines fn $name - it may only construct, wire and start (plus $MAIN_ALLOWED_FNS)
    $text"
				;;
			esac
		done

	grep -n '^[[:space:]]*\(pub \)\?\(struct\|enum\|trait\|impl\)[[:space:]]' src/main.rs 2>/dev/null |
		while IFS= read -r hit; do
			record "check 4 (composition root, I1): src/main.rs:${hit%%:*}: main.rs defines a type - it implements nothing
    ${hit#*:}"
		done
}

# --- Check 5: web is thin (I6), mechanical part -----------------------------
# The HTTP server lives in web and nowhere else.
check_web_is_thin() {
	grep -rn --include='*.rs' -e 'axum' -e 'TcpListener' src 2>/dev/null |
		grep -v -e '^src/web/' -e '^src/web\.rs:' |
		while IFS= read -r hit; do
			record "check 5 (web is thin, I6): ${hit%%:*}:$(printf '%s' "$hit" | cut -d: -f2): HTTP server code outside the web module - web owns the server, routing and (de)serialization
    ${hit#*:*:}"
		done
}

check_path_secrecy
check_dependency_edges
check_config_locality
check_composition_root
check_web_is_thin

echo "docs/ARCHITECTURE.md section 8 - mechanical enforcement"
echo
echo "  Verified mechanically by this script:"
echo "    1. Path secrecy (I3)         - no OS path, no device *_PATH env read, outside device/."
echo "    2. Dependency edges (I5)     - every cross-module crate:: reference is in section 4's table."
echo "    3. Config locality (I2)      - the composition root reads no config and names no path."
echo
echo "  Verified only in part - the rest still needs a human reviewer:"
echo "    4. Composition root (I1)     - checked: main.rs defines no type and no function beyond"
echo "                                   main, init_logging and log_startup_banner. NOT checked:"
echo "                                   that the body of main only constructs, wires and starts."
echo "    5. Web is thin (I6)          - checked: no HTTP server outside web/, and (via check 2)"
echo "                                   web imports only rtc. NOT checked: that every handler body"
echo "                                   is free of SDP/media/HID logic and ends in an rtc call."
echo

if [ -s "$VIOLATIONS" ]; then
	count=$(grep -c '^check ' "$VIOLATIONS")
	echo "FAILED - $count violation(s):"
	echo
	sed 's/^/  /' "$VIOLATIONS"
	echo
	exit 1
fi

echo "PASSED - no violation found by the checks above."
