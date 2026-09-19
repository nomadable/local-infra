//! Native PostgreSQL SQL workspace.

pub mod catalog;
pub mod editor;
pub mod render;
pub mod results;
pub mod workspace;

pub use workspace::{run_in_terminal, run_open};
