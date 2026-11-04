//! Product name and URL scheme of the running Crew flavor.
//!
//! The binary's compiled environment chooses them (`UiConfig::branding`); every
//! user-visible product name and every copied link must follow it, or a Staging
//! or production app would label itself — and route copied links to — another
//! flavor's installation.

use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Branding {
    /// User-visible product name ("Crew", "Crew Staging", …).
    pub app_name: &'static str,
    /// The URL scheme this flavor registers with the OS.
    pub link_scheme: &'static str,
}

impl Branding {
    pub const PRODUCTION: Self = Self { app_name: "Crew", link_scheme: "comet" };
}

/// Inbound schemes still routed, so links copied from any flavor keep opening.
pub const ACCEPTED_LINK_SCHEMES: [&str; 3] = ["comet", "comet-staging", "crew-upstream-canary"];

static BRANDING: OnceLock<Branding> = OnceLock::new();

/// Set once at app start; later calls are ignored.
pub fn set(branding: Branding) {
    let _ = BRANDING.set(branding);
}

/// The running flavor's branding (production when the app never set one).
pub fn current() -> Branding {
    BRANDING.get().copied().unwrap_or(Branding::PRODUCTION)
}

pub fn app_name() -> &'static str {
    current().app_name
}
