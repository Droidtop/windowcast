//! Secure-screen helper: a SYSTEM binary that brokers secure-desktop
//! access for windowcast hosts. See docs/SECURITY.md and docs/BACKENDS.md,
//! "A locked host" section.

pub mod client;
pub mod desktop;
pub mod executor;
pub mod operations;
pub mod permission;
pub mod pipe;

pub use client::{Client, SecureScreenError, SecureScreenStatus};

/// The name of the named pipe used between host and helper.
pub const PIPE_NAME: &str = r"\\.\pipe\windowcast-secure";

/// Registry path for the permission gate.
pub const REG_KEY: &str = r"SOFTWARE\windowcast\SecureScreen";

/// Registry value for the enabled flag.
pub const REG_VALUE_ENABLED: &str = "Enabled";

/// Registry value that records the SID of the granting user.
pub const REG_VALUE_GRANT_SID: &str = "GrantSid";
