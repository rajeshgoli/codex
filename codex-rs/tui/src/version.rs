/// The current Codex CLI version as embedded at compile time.
pub const CODEX_CLI_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Fork installations must be rebuilt locally, never replaced by upstream installers.
#[cfg(not(debug_assertions))]
pub(crate) const IS_FORK_BUILD: bool = true;
