//! Daemon core shared by the `hsind` binary and the standalone (daemon-less)
//! engine embedded in the `hsin` CLI.
// The modules predate the library split and are public only so the binary and
// embedded CLI can share one implementation. Keep the temporary lint scope
// here until that internal library API is narrowed.
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::module_name_repetitions
)]

pub mod app;
pub mod config;
pub mod crypto;
pub mod db;
pub mod error;
pub mod model;
pub mod paths;
pub mod proxy;
pub mod rpc;
pub mod service;
