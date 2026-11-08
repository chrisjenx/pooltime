//! Test-only helpers shared by unit tests that capture `tracing` output.

use std::sync::Once;

use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::Interest;
use tracing::{Event, Metadata, Subscriber};

/// Global fallback subscriber that records nothing but never lets a callsite be cached as
/// disabled.
///
/// Tracing caches a per-callsite `Interest` process-wide. Log-capture tests install a
/// thread-local subscriber with `with_default`, but when another test thread hits the same
/// callsite while it has no subscriber (or races the dispatcher registration), the callsite is
/// cached as `never` and the capturing test silently misses its events. `Interest::sometimes()`
/// forces every callsite to consult the current thread's dispatcher on each call instead.
struct AlwaysAsk;

impl Subscriber for AlwaysAsk {
    fn register_callsite(&self, _: &'static Metadata<'static>) -> Interest {
        Interest::sometimes()
    }
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        false
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, _: &Event<'_>) {}
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

/// Install [`AlwaysAsk`] as the process-global default subscriber, once per test binary.
///
/// Call this at the start of every test that uses `tracing::subscriber::with_default` to capture
/// output, and before anything that might install a different global subscriber (the first
/// global install wins; later ones are ignored).
pub(crate) fn ensure_callsites_stay_enabled() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = tracing::subscriber::set_global_default(AlwaysAsk);
    });
}
