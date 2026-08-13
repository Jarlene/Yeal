//! Platform adapters for desktop and browser computer use.
//!
//! - [`macos`]: native macOS Accessibility (AX) backend (cfg macOS).
//! - [`windows`]: native Windows UI Automation (UIA) backend (cfg Windows).
//! - [`linux`]: native Linux AT-SPI2 backend (cfg Linux).
//! - [`cdp`]: browser backend over the Chrome DevTools Protocol (all platforms).
//! - [`CompositeBackend`]: merges the desktop backend with the CDP backend
//!   into one multi-root forest with stable root refs.
//!
//! [`native_backend`] selects the platform-appropriate desktop backend and,
//! when a CDP endpoint is configured, wraps everything in a
//! [`CompositeBackend`].

mod composite;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;
#[cfg(target_os = "linux")]
pub mod linux;
pub mod cdp;

pub use composite::{CompositeBackend, RootRefAllocator};

use std::path::PathBuf;
use std::sync::Arc;

use crate::backend::UiBackend;

/// Environment-driven configuration for native backends.
#[derive(Debug, Clone, Default)]
pub struct ComputerUseConfig {
    /// CDP remote-debugging port of a running Chromium-family browser.
    /// Mirrors the `COMPUTER_USE_CDP_PORT` environment variable.
    pub cdp_port: Option<u16>,
    /// Path to a browser executable used by `launch_browser` when no CDP
    /// endpoint is already running. Mirrors `COMPUTER_USE_BROWSER_PATH`.
    pub browser_path: Option<PathBuf>,
    /// Prohibit foreground activation / physical input. Mirrors
    /// `COMPUTER_USE_HEADLESS`.
    pub headless: bool,
}

impl ComputerUseConfig {
    /// Build a config from environment variables:
    /// `COMPUTER_USE_CDP_PORT`, `COMPUTER_USE_BROWSER_PATH`,
    /// `COMPUTER_USE_HEADLESS`.
    pub fn from_env() -> Self {
        let cdp_port = std::env::var("COMPUTER_USE_CDP_PORT")
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .filter(|port| *port > 0);
        let browser_path = std::env::var("COMPUTER_USE_BROWSER_PATH")
            .ok()
            .map(PathBuf::from);
        let headless = std::env::var("COMPUTER_USE_HEADLESS")
            .ok()
            .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"));
        Self {
            cdp_port,
            browser_path,
            headless,
        }
    }
}

/// The platform-appropriate desktop backend. On platforms without a native
/// adapter this returns the explicit [`UnsupportedBackend`].
pub fn desktop_backend() -> Arc<dyn UiBackend> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(macos::MacosBackend::new(macos::MacosOptions {
            headless: false,
        }))
    }
    #[cfg(target_os = "windows")]
    {
        Arc::new(windows::WindowsBackend::new())
    }
    #[cfg(target_os = "linux")]
    {
        Arc::new(linux::LinuxBackend::new(linux::LinuxOptions {
            headless: false,
        }))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        use crate::backend::UnsupportedBackend;
        Arc::new(UnsupportedBackend)
    }
}

/// Build the full backend: desktop adapter plus an optional CDP browser
/// backend, merged by a [`CompositeBackend`].
pub fn native_backend(config: &ComputerUseConfig) -> Arc<dyn UiBackend> {
    let desktop = desktop_backend();
    let cdp = match config.cdp_port {
        Some(port) => Some(Arc::new(cdp::CdpBackend::new(cdp::CdpOptions {
            port,
            browser_path: config.browser_path.clone(),
            headless: config.headless,
        })) as Arc<dyn UiBackend>),
        None => None,
    };
    Arc::new(CompositeBackend::new(desktop, cdp))
}
