//! Product policy for the native Raft Computer installer. K owns the upgrade
//! transaction; this crate owns release selection and the Computer adapter.
pub mod artifact;
pub mod cli;
pub mod computer;
pub mod config;
pub mod host;
pub mod operation;
pub mod presence;
pub mod process;
pub mod report;
pub mod request;
pub mod runner;
pub mod shell_path;
pub mod source;
pub mod supervisor;
pub mod version;
pub mod world;

pub use k_carrier::{Error, Result};
