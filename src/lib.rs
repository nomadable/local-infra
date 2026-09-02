//! local-infra — shared development infrastructure, local and remote.
//!
//! `core` owns every behaviour; `cli` and `tui` are two presentations of the
//! same use cases, which is what keeps the headless surface complete
//! (PRD §6.2, principle 7).

pub mod cli;
pub mod core;
pub mod tui;
