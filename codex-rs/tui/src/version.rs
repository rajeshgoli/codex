/// The current Codex CLI version as embedded at compile time.
#[cfg(not(test))]
pub const CODEX_CLI_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Unit tests render upstream's `0.0.0` so snapshots stay identical to upstream. Fork builds
/// carry a real release number because the model service hides newer models from `0.0.0`.
#[cfg(test)]
pub const CODEX_CLI_VERSION: &str = "0.0.0";

/// Fork installations must be rebuilt locally, never replaced by upstream installers.
#[cfg(not(debug_assertions))]
pub(crate) const IS_FORK_BUILD: bool = true;
