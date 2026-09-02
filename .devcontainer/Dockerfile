# syntax=docker/dockerfile:1
# The `syntax` line above turns on two BuildKit features used below: cache
# mounts (`--mount=type=cache`), which keep cargo's downloads and build
# output between image rebuilds, and multi-stage builds, which let the dev
# CLIs below compile in a separate stage that runs in parallel with the
# unrelated installs later in this file instead of blocking them.

# The plain Alpine devcontainer base.
FROM mcr.microsoft.com/devcontainers/base:alpine AS base

# Toolchain deps the base image lacks: a C linker for rustc, plus pkg-config /
# OpenSSL headers commonly needed by Rust crates. clang-dev + linux-headers
# are for the `v4l` crate's bindgen-based sys crate. libva-dev is for the
# `libva-rs` crate's bindgen-based build script and for linking against libva.
RUN apk add --no-cache build-base pkgconf openssl-dev clang-dev linux-headers libva-dev

# zig is the actual cross-compiler cargo-zigbuild (installed below via cargo)
# shells out to for test-on-device.sh's x86_64-unknown-linux-musl builds.
# Installed from the official tarball (checksum-verified) and pinned to
# 0.11.0, NOT Alpine's `zig` apk package: Alpine's package tracks whatever
# version its rolling repo currently has, and 0.12.0 switched the default
# x86_64 ELF linker from LLD to zig's own self-hosted linker. That linker
# silently drops rustc's standard `-Wl,-O1` release-build hint instead of
# acting on it, logging a harmless but confusing "ignoring deprecated linker
# optimization setting" warning on every release build. Pinning to the last
# LLD-based release avoids the warning and stops the version (and this
# behavior) from silently drifting on the next image rebuild.
ARG TARGETARCH
RUN case "$TARGETARCH" in \
        arm64) ZIG_ARCH=aarch64; ZIG_SHA=956eb095d8ba44ac6ebd27f7c9956e47d92937c103bf754745d0a39cdaa5d4c6 ;; \
        amd64) ZIG_ARCH=x86_64; ZIG_SHA=2d00e789fec4f71790a6e7bf83ff91d564943c5ee843c5fd966efc474b423047 ;; \
        *) echo "Unsupported arch: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && curl -fsSL -o /tmp/zig.tar.xz "https://ziglang.org/download/0.11.0/zig-linux-${ZIG_ARCH}-0.11.0.tar.xz" \
    && echo "${ZIG_SHA}  /tmp/zig.tar.xz" | sha256sum -c - \
    && mkdir -p /usr/local/zig \
    && tar xf /tmp/zig.tar.xz -C /usr/local/zig --strip-components=1 \
    && rm /tmp/zig.tar.xz \
    && ln -s /usr/local/zig/zig /usr/local/bin/zig

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
# browser-test driver (agent-browser). Built in their own stage (from `base`)
# so BuildKit can run this compile-heavy work in parallel with the unrelated
# installs in the main stage below, instead of making them wait behind it.
#
# `cargo install` with several crates in one command builds them one at a
# time, and even a single crate's own dependency graph often narrows to one
# compileable crate at a time (e.g. everything blocked on a proc-macro crate
# like `serde`/`syn` finishing) - both leave cores idle. Running the three
# installs as separate, concurrent processes gives cargo three independent
# dependency graphs to pull work from at once, so a stall in one doesn't
# stall the others.
#
# Each install gets its own `--target-dir` cache mount rather than sharing
# one: cargo holds an exclusive lock on a target dir while building, so
# concurrent installs sharing one would just serialize behind that lock,
# undoing the point of running them concurrently. The crates.io registry/git
# cache is shared across all three, and persists across future image
# rebuilds too, since cargo already manages safe concurrent access to it and
# a cache mount survives even when the layer using it gets rebuilt.
FROM base AS tools
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/usr/local/cargo-target-nextest \
    --mount=type=cache,target=/usr/local/cargo-target-bacon \
    --mount=type=cache,target=/usr/local/cargo-target-agent-browser \
    cargo install --locked --target-dir /usr/local/cargo-target-nextest cargo-nextest & pid1=$!; \
    cargo install --locked --target-dir /usr/local/cargo-target-bacon bacon & pid2=$!; \
    cargo install --locked --target-dir /usr/local/cargo-target-agent-browser agent-browser & pid3=$!; \
    wait "$pid1" && wait "$pid2" && wait "$pid3"

FROM base

# Chromium for the agent-browser browser test. Chrome for Testing has no arm64
# build, so the test drives this system Chromium at /usr/bin/chromium.
# github-cli (gh) is here too, for publishing/PR work from inside the container.
# jq is used below to edit the Claude Code user prefs file (.claude.json).
RUN apk add --no-cache chromium github-cli jq

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
# ponytail comes from its own marketplace (not the official one above).
RUN claude plugin marketplace add DietrichGebert/ponytail && \
    (claude plugin install rust-analyzer-lsp@claude-plugins-official || echo "Warning: plugin install failed, continuing build")
    # claude plugin marketplace add anthropics/claude-plugins-official && \
    # (claude plugin install ponytail@ponytail || echo "Warning: plugin install failed, continuing build") && \
# Stop Claude Code from auto-installing its VS Code extension when `claude`
# runs in the integrated terminal. This is a /config toggle, not a
# settings.json field, so it lives in .claude.json instead. Baked into the
# image (not just set live) so it also lands in the fresh claude-config
# volume that gets created for any new clone/rebuild of this devcontainer.
RUN jq '.autoInstallIdeExtension = false' "$CLAUDE_CONFIG_DIR/.claude.json" > /tmp/claude.json.tmp \
    && mv /tmp/claude.json.tmp "$CLAUDE_CONFIG_DIR/.claude.json"

# Statusline badge (context window + rate limit bars), baked in the same way
# for the same reason: settings.json lives on the claude-config volume, so
# this only needs to happen once per fresh volume, not on every rebuild.
RUN jq '.statusLine = {"type": "command", "command": "bash .devcontainer/statusline-command.sh"}' \
      "$CLAUDE_CONFIG_DIR/settings.json" > /tmp/settings.json.tmp \
    && mv /tmp/settings.json.tmp "$CLAUDE_CONFIG_DIR/settings.json"

USER root

# Pull the dev CLIs in from the `tools` stage above. Placed last so that
# everything above (Chromium, Claude Code, its plugins) builds in parallel
# with that stage's compile instead of waiting for it to finish first.
COPY --from=tools --chown=vscode:vscode \
    /usr/local/cargo/bin/cargo-nextest \
    /usr/local/cargo/bin/bacon \
    /usr/local/cargo/bin/agent-browser \
    /usr/local/cargo/bin/
