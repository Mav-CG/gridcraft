//! Fork Mav-CG/gridcraft: client for Excel-style RTD servers (COM `IRtdServer`) behind a safe API,
//! for `=RTD()` on Windows. The only crate in the workspace with `unsafe` (COM FFI); other platforms
//! get the types and no client. Spec: docs/superpowers/specs/2026-10-12-rtd-design.md
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::RtdClient;

/// The RTD servers `=RTD()` may start (spec §3.2); any other ProgID is `#N/A` without touching COM.
pub const ALLOWED: &[&str] = &["srv.rtd", "rtdtrading.rtdserver"];

pub fn is_allowed(prog_id: &str) -> bool {
    ALLOWED.iter().any(|a| a.eq_ignore_ascii_case(prog_id))
}

/// One RTD topic: the server's ProgID, the (unused, local-only) server name and the topic strings.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Topic {
    pub prog_id: String,
    pub server: String,
    pub strings: Vec<String>,
}

/// A topic's value as the server sent it.
#[derive(Clone, Debug, PartialEq)]
pub enum RtdValue {
    Empty,
    Number(f64),
    Text(String),
    Bool(bool),
    /// An error value or a type `=RTD()` can't show.
    Error,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_two_servers_are_allowed() {
        assert!(is_allowed("srv.rtd"));
        assert!(is_allowed("RTDTrading.RtdServer"));
        assert!(!is_allowed("excel.rtd"));
        assert!(!is_allowed(""));
    }
}
