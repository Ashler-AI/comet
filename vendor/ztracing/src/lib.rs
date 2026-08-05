//! Apache-licensed compatibility surface for GPUI's `sum_tree` dependency.
//!
//! `sum_tree` only consumes the standard `#[instrument]` attribute. Re-export
//! the upstream `tracing` implementation directly so the fork does not link
//! Zed's GPL-licensed tracing wrapper crates.

#![forbid(unsafe_code)]

pub use tracing::instrument;
