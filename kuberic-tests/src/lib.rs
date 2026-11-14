//! Shared fixtures and mock backends for Kuberic integration tests.

pub mod controller;
pub mod fm;
#[cfg(target_os = "linux")]
pub mod process;
