//! Custom panic hook that sends the panic information to a tracing
//! span

#![deny(rustdoc::broken_intra_doc_links, rustdoc::bare_urls, rust_2018_idioms)]
#![warn(
    missing_debug_implementations,
    clippy::explicit_iter_loop,
    clippy::use_self,
    clippy::clone_on_ref_ptr,
    clippy::todo,
    clippy::dbg_macro,
    unused_crate_dependencies
)]

// Workaround for "unused crate" lint false positives.
use workspace_hack as _;

use std::{collections::HashMap, fmt, panic, sync::Arc};

use metric::U64Counter;
use observability_deps::tracing::{error, warn};
use panic::PanicInfo;

type PanicFunctionPtr = Arc<Box<dyn Fn(&PanicInfo<'_>) + Sync + Send + 'static>>;

/// RAII guard that installs a custom panic hook to send panic
/// information to tracing.
///
/// Upon construction registers a custom panic
/// hook which sends the panic to tracing first, before calling any
/// prior panic hook.
///
/// Upon drop, restores the pre-existing panic hook
#[derive(Default)]
pub struct SendPanicsToTracing {
    /// The previously installed panic hook -- Note it is wrapped in an
    /// `Option` so we can `.take` it during the call to `drop()`;
    old_panic_hook: Option<PanicFunctionPtr>,
}

impl SendPanicsToTracing {
    pub fn new() -> Self {
        Self::new_inner(None)
    }

    /// Configure this panic handler to emit a panic count metric.
    ///
    /// The metric is named `thread_panic_count_total` and is incremented each
    /// time the panic handler is invoked.
    pub fn new_with_metrics(metrics: &metric::Registry) -> Self {
        let metrics = Metrics::new(metrics);
        Self::new_inner(Some(metrics))
    }

    fn new_inner(metrics: Option<Metrics>) -> Self {
        let current_panic_hook: PanicFunctionPtr = Arc::new(panic::take_hook());
        let old_panic_hook = Some(Arc::clone(&current_panic_hook));
        panic::set_hook(Box::new(move |info| {
            let panic_type = PanicType::classify(info);
            if let Some(metrics) = &metrics {
                metrics.inc(panic_type);
            }

            let location = info.location();
            error!(
                panic_type = panic_type.name(),
                panic_message = message(info),
                panic_file = location.map(|l| l.file()),
                panic_line = location.map(|l| l.line()),
                panic_column = location.map(|l| l.column()),
                "Thread panic",
            );

            current_panic_hook(info);
        }));

        Self { old_panic_hook }
    }
}

// can't derive because the function pointer doesn't implement Debug
impl fmt::Debug for SendPanicsToTracing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SendPanicsToTracing").finish()
    }
}

impl Drop for SendPanicsToTracing {
    fn drop(&mut self) {
        if std::thread::panicking() {
            warn!("Can't reset old panic hook as we are currently panicking");
            return;
        }

        if let Some(old_panic_hook) = self.old_panic_hook.take() {
            // since `old_panic_hook` is an `Arc` - at this point it
            // should have two references -- the captured closure as
            // well as `self`.

            // Temporarily install a dummy hook that does nothing. We
            // need to release the ref count in the closure of the
            // panic handler.
            panic::set_hook(Box::new(|_| {
                println!("This panic hook should 'never' be called");
            }));

            if let Ok(old_panic_hook) = Arc::try_unwrap(old_panic_hook) {
                panic::set_hook(Box::new(old_panic_hook))
            } else {
                // Should not happen -- but could if the panic handler
                // was still running while this code is being executed
                warn!("Can't reset old panic hook, old hook still has more than one reference");
            }
        } else {
            // This is a "shouldn't happen" type error
            warn!("Can't reset old panic hook, old hook was None...");
        }
    }
}

/// Ensure panics are fatal events by exiting the process with an exit code of
/// 1 after calling the existing panic handler, if any.
pub fn make_panics_fatal() {
    let existing = panic::take_hook();

    panic::set_hook(Box::new(move |info| {
        // Call the existing panic hook.
        existing(info);
        // Exit the process.
        //
        // NOTE: execution may not reach this point if another hook
        // kills the process first.
        std::process::exit(1);
    }));
}

/// Panic type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum PanicType {
    /// Counter for unknown panics.
    Unknown,

    /// Counter for "offset"/"offset overflow" panics.
    ///
    /// These are likely caused due too overly large string columns in Arrow.
    OffsetOverflow,
}

impl PanicType {
    fn all() -> &'static [Self] {
        &[Self::Unknown, Self::OffsetOverflow]
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::OffsetOverflow => "offset_overflow",
        }
    }

    fn classify(panic_info: &PanicInfo<'_>) -> Self {
        match message(panic_info) {
            Some("offset overflow" | "offset") => Self::OffsetOverflow,
            _ => Self::Unknown,
        }
    }
}

/// Extract string message from [`PanicInfo`]
fn message<'a>(panic_info: &'a PanicInfo<'a>) -> Option<&'a str> {
    let payload_any = panic_info.payload();

    payload_any
        .downcast_ref::<&str>()
        .copied()
        .or(payload_any.downcast_ref::<String>().map(|s| s.as_str()))
}

/// Metrics used for panics.
#[derive(Debug)]
struct Metrics {
    /// Counter for different panic types.
    counters: HashMap<PanicType, U64Counter>,
}

impl Metrics {
    fn new(metrics: &metric::Registry) -> Self {
        let metric = metrics.register_metric::<U64Counter>(
            "thread_panic_count",
            "number of thread panics observed",
        );

        Self {
            counters: PanicType::all()
                .iter()
                .map(|t| (*t, metric.recorder(&[("type", t.name())])))
                .collect(),
        }
    }

    fn inc(&self, panic_type: PanicType) {
        self.counters
            .get(&panic_type)
            .expect("all types covered")
            .inc(1);
    }
}

#[cfg(test)]
mod tests {
    use std::panic::panic_any;

    use metric::{Attributes, Metric};
    use test_helpers::{maybe_start_logging, tracing::TracingCapture};

    use super::*;

    fn assert_count(metrics: &metric::Registry, t: &'static str, count: u64) {
        let got = metrics
            .get_instrument::<Metric<U64Counter>>("thread_panic_count")
            .expect("failed to read metric")
            .get_observer(&Attributes::from(&[("type", t)]))
            .expect("failed to get observer")
            .fetch();
        assert_eq!(got, count);
    }

    #[test]
    fn test_panic_counter_and_logging() {
        maybe_start_logging();

        let metrics = metric::Registry::default();
        let capture = Arc::new(TracingCapture::new());
        let guard = SendPanicsToTracing::new_with_metrics(&metrics);

        assert_count(&metrics, "offset_overflow", 0);
        assert_count(&metrics, "unknown", 0);

        let capture2 = Arc::clone(&capture);
        std::thread::spawn(move || {
            capture2.register_in_current_thread();
            panic!("it's bananas");
        })
        .join()
        .expect_err("wat");

        let capture2 = Arc::clone(&capture);
        std::thread::spawn(move || {
            capture2.register_in_current_thread();
            panic!("offset");
        })
        .join()
        .expect_err("wat");

        let capture2 = Arc::clone(&capture);
        std::thread::spawn(move || {
            capture2.register_in_current_thread();
            let s = String::from("offset overflow");
            panic!("{}", s);
        })
        .join()
        .expect_err("wat");

        let capture2 = Arc::clone(&capture);
        std::thread::spawn(move || {
            capture2.register_in_current_thread();
            panic_any(1);
        })
        .join()
        .expect_err("wat");

        drop(guard);
        let capture2 = Arc::clone(&capture);
        std::thread::spawn(move || {
            capture2.register_in_current_thread();
            panic!("no guard");
        })
        .join()
        .expect_err("wat");

        assert_count(&metrics, "offset_overflow", 2);
        assert_count(&metrics, "unknown", 2);

        assert_eq!(
            capture.to_string(),
            "level = ERROR; message = Thread panic; panic_type = \"unknown\"; panic_message = \"it's bananas\"; panic_file = \"panic_logging/src/lib.rs\"; panic_line = 242; panic_column = 13; \n\
             level = ERROR; message = Thread panic; panic_type = \"offset_overflow\"; panic_message = \"offset\"; panic_file = \"panic_logging/src/lib.rs\"; panic_line = 250; panic_column = 13; \n\
             level = ERROR; message = Thread panic; panic_type = \"offset_overflow\"; panic_message = \"offset overflow\"; panic_file = \"panic_logging/src/lib.rs\"; panic_line = 259; panic_column = 13; \n\
             level = ERROR; message = Thread panic; panic_type = \"unknown\"; panic_file = \"panic_logging/src/lib.rs\"; panic_line = 267; panic_column = 13; "
        );
    }
}
