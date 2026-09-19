//! Native PostgreSQL workbench core shared by the CLI and TUI.

pub mod catalog;
pub mod connection;
pub mod export;
pub mod history;
pub mod profile;
pub mod runner;
pub mod statement;
pub use profile::{
    AccessMode, PasswordUpdate, ProfileSpec, QueryHistoryEntry, ResolvedSqlConnection,
    SqlConnectionProfile, SqlConnectionSource, SqlEndpoint, TlsMode,
};
