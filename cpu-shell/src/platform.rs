//! Platform capabilities as a trait.
//!
//! Everything here is deliberately tiny: the GPU-free renderer needs the OS
//! only for "reveal in file manager", clipboard and (on Windows) the default
//! viewer registration. Video/DPI/IME are handled by winit.

use std::path::Path;
use std::process::Command;

pub trait Platform: Send + Sync {
    fn name(&self) -> &'static str;
    fn reveal(&self, path: &Path);
    fn copy_text(&self, text: &str) -> bool;
    fn register_default_viewer(&self, _exe: &Path) -> bool {
        false
    }
}

pub struct LinuxPlatform;
pub struct MacPlatform;
pub struct WindowsPlatform;

impl Platform for LinuxPlatform {
    fn name(&self) -> &'static str {
        "linux"
    }

    fn reveal(&self, path: &Path) {
        let dir = if path.is_dir() {
            path.to_path_buf()
        } else {
            path.parent().map(|p| p.to_path_buf()).unwrap_or_default()
        };
        let _ = Command::new("xdg-open").arg(dir).spawn();
    }

    fn copy_text(&self, text: &str) -> bool {
        arboard::Clipboard::new()
            .and_then(|mut c| c.set_text(text.to_string()))
            .is_ok()
    }
}

impl Platform for MacPlatform {
    fn name(&self) -> &'static str {
        "macos"
    }

    fn reveal(&self, path: &Path) {
        let _ = Command::new("open").arg("-R").arg(path).spawn();
    }

    fn copy_text(&self, text: &str) -> bool {
        arboard::Clipboard::new()
            .and_then(|mut c| c.set_text(text.to_string()))
            .is_ok()
    }
}

impl Platform for WindowsPlatform {
    fn name(&self) -> &'static str {
        "windows"
    }

    fn reveal(&self, path: &Path) {
        // explorer wants the /select,<path> form and tolerates quotes
        let arg = format!("/select,{}", path.display());
        let _ = Command::new("explorer").arg(arg).spawn();
    }

    fn copy_text(&self, text: &str) -> bool {
        arboard::Clipboard::new()
            .and_then(|mut c| c.set_text(text.to_string()))
            .is_ok()
    }

    fn register_default_viewer(&self, _exe: &Path) -> bool {
        // TODO: port win32.rs register_default_viewer (HKCU ProgID + OpenWithProgids)
        false
    }
}

pub fn current() -> Box<dyn Platform> {
    #[cfg(target_os = "windows")]
    {
        Box::new(WindowsPlatform)
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(MacPlatform)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        Box::new(LinuxPlatform)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn current_platform_matches_target() {
        let platform = super::current();
        let expected = if cfg!(windows) {
            "windows"
        } else if cfg!(target_os = "macos") {
            "macos"
        } else {
            "linux"
        };
        assert_eq!(platform.name(), expected);
    }

    #[test]
    fn copy_text_does_not_panic_headless() {
        // Clipboard access may be unavailable in CI; it must degrade to false,
        // never panic.
        let platform = super::current();
        let _ = platform.copy_text("turboview-cpu");
    }
}
