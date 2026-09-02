//! Use-case layer. Owns every behaviour in the product.
//!
//! `cli` and `tui` are presentation only: anything either of them can do, the
//! other can do too, because both call exactly these functions (PRD §6.2,
//! principle 7).

pub mod activity;
pub mod backup;
pub mod bucket;
pub mod config;
pub mod ctx;
pub mod database;
pub mod discovery;
pub mod docker;
pub mod doctor;
pub mod engine;
pub mod error;
pub mod exec;
pub mod minio;
pub mod model;
pub mod pg;
pub mod plan;
pub mod progress;
pub mod secrets;
pub mod ssh;
pub mod store;
pub mod target;
pub mod tunnel;
pub mod util;

pub use ctx::Ctx;
pub use error::{Diagnostic, Error, Result};
