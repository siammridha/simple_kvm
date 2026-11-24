# Everything shared by all projects (Rust, nextest, bacon, agent-browser,
# Chromium, gh, Claude Code) comes from devbase; see base/Dockerfile in
# github.com/siammridha/devcontainer-template. Only this project's extras are
# added here.
FROM ghcr.io/siammridha/devbase:latest

# clang-dev + linux-headers: bindgen for the `v4l` sys crate. libva-dev:
# `libva-rs` bindgen and linking.
RUN apk add --no-cache clang-dev linux-headers libva-dev

# zig for test-on-device.sh's x86_64-unknown-linux-musl builds. Pinned to
# 0.11.0 from the official tarball, not Alpine's apk: 0.12.0 replaced LLD with
# zig's own linker, which warns on every release build about rustc's -Wl,-O1.
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
