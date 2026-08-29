mod capture;
mod device;
mod hid;
mod rtc;
mod web;

use std::sync::Arc;

use tracing_subscriber::EnvFilter;

use capture::engine::CaptureCard;
use device::CaptureDevice;
use hid::Hid;

#[tokio::main]
async fn main() {
    init_logging();
    log_startup_banner(env!("CARGO_PKG_VERSION"));
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "simple_kvm starting");

    // --- Capture: the card is never opened automatically right here at
    // startup. Opening it unprompted has reliably crashed the real hardware
    // this targets right at boot (see README's "boot-crash" known issue) -
    // `Device<CaptureDriver>`'s presence task (spawned by `CaptureDevice::
    // spawn` below) deliberately never probes the very first time it finds
    // the device already present, for exactly that reason. `CaptureCard`
    // owns the capture settings and the UI-facing device state; both are
    // in-memory only, and nothing here is ever read from or written to
    // disk. Nothing here ever sees the raw device path either - the device
    // reads it from its own config. ---
    let capture_card = Arc::new(CaptureCard::new(CaptureDevice::spawn()));

    // --- Serial: same soft-unavailable treatment as capture. `Hid` owns
    // its own device, queue, drain worker and mouse mode; commands sent
    // before its port is open queue up rather than being lost, so nothing
    // here holds up the HTTP page starting. ---
    let hid = Hid::spawn();

    let rtc = rtc::Rtc::new(capture_card, hid);

    // --- Web: owns the port, the listener and every route. Runs until the
    // process ends, which is what keeps the whole service alive. ---
    web::serve(rtc).await;
}

/// Used only when `RUST_LOG` isn't set. Everything else stays at `info`,
/// but keystroke/click handling logs at `debug` by default so input-lag
/// reports are visible in the log immediately, with no configuration step
/// needed first - setting `RUST_LOG` explicitly still overrides this
/// entirely, same as any `EnvFilter`.
const DEFAULT_LOG_FILTER: &str = "info,simple_kvm::rtc::session=debug,simple_kvm::hid::writer=debug";

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// A loud, hand-colored banner printed straight to stdout (not through
/// `tracing`, so it isn't buried under a timestamp/level/target prefix like
/// every other line) - meant to jump out visually when scrolling past
/// hundreds of dense per-frame/per-packet debug lines to find where the
/// service actually (re)started.
fn log_startup_banner(version: &str) {
    let line = format!("simple_kvm v{version} — service started");
    let bar = "=".repeat(line.chars().count() + 4);
    println!("\x1b[1;32m{bar}\x1b[0m");
    println!("\x1b[1;32m  {line}\x1b[0m");
    println!("\x1b[1;32m{bar}\x1b[0m");
}
