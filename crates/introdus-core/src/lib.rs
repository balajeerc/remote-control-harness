//! `introdus-core` — shared library for the introdus control plane.
//!
//! Houses the pieces that both the CLI orchestration and the TUI need:
//! the typed `.env` config, filesystem paths, podman object naming, the
//! embedded container-side bash assets, the coding-agent registry, and thin
//! `podman`/`tmux`/`git` process wrappers. Modules land milestone by milestone
//! (see PLAN.md).

pub mod agents;
pub mod assets;
pub mod config;
pub mod containers;
pub mod egress;
pub mod env_file;
pub mod hostnet;
pub mod names;
pub mod notify;
pub mod paths;
pub mod podman;
pub mod ports;
pub mod process;
pub mod remote;
pub mod session;
pub mod sshconfig;
pub mod tmux;

pub use config::Config;

/// The crate/binary version, sourced from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The on-PATH binary name, used in generated help text and banners.
pub const BIN_NAME: &str = "introdus";
