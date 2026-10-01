/// Gives an instrumentation harness a stable, low-cardinality name for the
/// command about to run -- the outermost tracing span's name and its
/// `command` field (ADR-0073). Implemented by the CLI arg types that already
/// carry "which operation is this" (`JobType`, `KeyringCommands`), not by
/// `Job` or `KeyringEntry` themselves, since gather/run are already inside
/// the span by the time either trait's methods are called.
pub(crate) trait Observable {
    fn command_name(&self) -> &'static str;
}
