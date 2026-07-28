//! Daemon core shared by the `hsind` binary and the standalone (daemon-less)
//! engine embedded in the `hsin` CLI.
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
