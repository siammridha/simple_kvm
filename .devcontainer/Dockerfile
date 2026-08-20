# The plain Alpine devcontainer base.
FROM mcr.microsoft.com/devcontainers/base:alpine

# Toolchain deps the base image lacks: a C linker for rustc, plus pkg-config /
# OpenSSL headers commonly needed by Rust crates. clang-dev + linux-headers
# are for the `v4l` crate's bindgen-based sys crate; nasm speeds up the
# `openh264` software encoder, which matters on the target's weak CPU.
RUN apk add --no-cache build-base pkgconf openssl-dev clang-dev linux-headers nasm

# Rust via rustup, in the same shared paths the official rust image uses (so
# the cargo-cache volume mount in devcontainer.json keeps working).
ENV CARGO_HOME=/usr/local/cargo \
    RUSTUP_HOME=/usr/local/rustup \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path \
        --default-toolchain stable --profile minimal \
        -c rustfmt -c clippy -c rust-src -c rust-analyzer \
    && chown -R vscode:vscode /usr/local/cargo /usr/local/rustup

# Dev CLIs: the test runner (cargo nextest), the file watcher (bacon), and the
# browser-test driver (agent-browser). Installed into the shared cargo paths so
# the cargo-cache volume keeps them, then chowned like the rest of the toolchain.
RUN cargo install --locked cargo-nextest bacon agent-browser \
    && chown -R vscode:vscode /usr/local/cargo

# Chromium for the agent-browser browser test. Chrome for Testing has no arm64
# build, so the test drives this system Chromium at /usr/bin/chromium.
# github-cli (gh) is here too, for publishing/PR work from inside the container.
RUN apk add --no-cache chromium github-cli

# Keep ALL Claude state (including what would otherwise be ~/.claude.json) in
# one directory, so a single volume mount persists login + sessions.
ENV CLAUDE_CONFIG_DIR=/home/vscode/.claude

# The native installer is a PER-USER install: it drops the binary in
# ~/.local/bin for whoever runs it. So Claude Code (and the user-scope plugin
# below) are installed as the vscode user, and ~/.local/bin is put on PATH.
# Updates happen via `claude update` / the background auto-updater, NOT apk.
ENV PATH=/home/vscode/.local/bin:$PATH
USER vscode

# Claude Code via Anthropic's native installer. Needs bash + the musl runtime
# deps (libgcc, libstdc++, ripgrep) installed in the first apk layer above.
RUN curl -fsSL https://claude.ai/install.sh | bash

# The rust-analyzer-lsp plugin is user-scope (CLAUDE_CONFIG_DIR), so it's
# installed here as the same vscode user that owns the Claude Code binary.
RUN claude plugin marketplace add anthropics/claude-plugins-official && \
    claude plugin install rust-analyzer-lsp@claude-plugins-official || \
    echo "Warning: plugin install failed, continuing build"
USER root