use std::backtrace::Backtrace;

use tracing_error::SpanTrace;

/// Installs a panic hook that captures a full backtrace plus the active
/// `tracing` span stack (so a panic mid-batch still carries whatever
/// identity/mailbox/uid/step context the enclosing spans set, ADR-0073) into
/// one durable structured log event, then chains to whatever hook was
/// previously installed -- today's stderr crash output is unchanged; this
/// only adds a structured record alongside it. Safe to call given this
/// crate's default (non-`abort`) panic strategy: a panic inside a spawned
/// task is already caught by that task's `JoinHandle` and does not crash the
/// process; installing this hook doesn't change that.
pub(crate) fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = Backtrace::force_capture();
        let span_trace = SpanTrace::capture();
        tracing::error!(
            panic = %info,
            backtrace = %backtrace,
            span_trace = %span_trace,
            "panic"
        );
        previous(info);
    }));
}
