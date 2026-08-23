#!/bin/bash
# Splits Claude Code's login token out of the claude-config volume into its
# own claude-auth volume, so claude-config (sessions, history, cache) can be
# deleted and rebuilt without losing login. Runs on every container start.
#
# .credentials.json in ~/.claude is made a symlink into ~/.claude-auth. If a
# real file shows up there instead (a fresh login, or Claude Code replacing
# the symlink outright when it writes the file), it's moved into the auth
# volume and the symlink is put back.
set -euo pipefail

AUTH_DIR=/home/vscode/.claude-auth
CRED_FILE=/home/vscode/.claude/.credentials.json
AUTH_CRED_FILE="$AUTH_DIR/.credentials.json"

sudo chown vscode:vscode "$AUTH_DIR"

if [ -e "$CRED_FILE" ] && [ ! -L "$CRED_FILE" ]; then
    mv "$CRED_FILE" "$AUTH_CRED_FILE"
fi

rm -f "$CRED_FILE"
ln -s "$AUTH_CRED_FILE" "$CRED_FILE"
