mod capture;
mod device;
mod hid;
mod rtc;
mod web;

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    let log_level = init_logging();
    log_startup_banner(env!("CARGO_PKG_VERSION"));
    tracing::info!(log_level = %log_level, "simple_kvm starting");

    // --- rtc: a composition root in its own right - building it is what
    // constructs and starts its own capture engine and HID module (see
    // `Rtc::spawn`). ---
    let rtc = rtc::Rtc::spawn();

    // --- Web: owns the port, the listener and every route. Runs until the
    // process ends, which is what keeps the whole service alive. ---
    web::serve(rtc).await;
}

fn init_logging() -> EnvFilter {
    let filter = EnvFilter::from_default_env();
    tracing_subscriber::fmt().with_env_filter(filter.clone()).init();
    filter
}

/// A loud, hand-colored banner printed straight to stdout (not through
/// `tracing`, so it isn't buried under a timestamp/level/target prefix like
/// every other line) - meant to jump out visually when scrolling past
/// hundreds of dense per-frame/per-packet debug lines to find where the
/// service actually (re)started.
fn log_startup_banner(version: &str) {
    let line = format!("simple_kvm v{version}");
    let bar = "=".repeat(line.chars().count() + 4);
    println!("\x1b[1;32m{bar}\x1b[0m");
    println!("\x1b[1;32m  {line}\x1b[0m");
    println!("\x1b[1;32m{bar}\x1b[0m");
}
