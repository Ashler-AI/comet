//! Which Crew this build is: the name people see and the URL scheme its
//! bundle registers. The binary chooses it (`apps/zeron/src/environment.rs`,
//! one entry per channel) and [`crate::run_app`] installs it before any
//! window, menu or link exists, so no screen spells a channel itself.

use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Product {
    /// The app menu, About, update prompts, the Windows title.
    pub name: &'static str,
    /// What the bundle registers (`CFBundleURLSchemes`): links this build
    /// copies open in this build.
    pub url_scheme: &'static str,
}

static PRODUCT: OnceLock<Product> = OnceLock::new();

/// Called once by [`crate::run_app`]. A second app in the same process keeps
/// the first one's product: one process is one bundle.
pub(crate) fn install(product: Product) {
    let _ = PRODUCT.set(product);
}

/// This build's product. Unit tests that never ran the app see [`TEST`].
pub fn current() -> Product {
    match PRODUCT.get() {
        Some(product) => *product,
        #[cfg(test)]
        None => TEST,
        #[cfg(not(test))]
        None => panic!("zeron_ui::run_app installs the product before anything reads it"),
    }
}

/// What the crate's own tests run as: a real channel, so links they build
/// round-trip through the same parsers the app uses.
#[cfg(test)]
pub(crate) const TEST: Product = Product {
    name: "Crew Staging",
    url_scheme: "comet-staging",
};
