pub mod compute_provider;
mod entity;
pub mod execution;
mod generation;
pub mod logging;
pub mod tools;

pub use entity::projects::burn_dir::project::BurnCentralProject;
pub use entity::projects::{CrateInfo, ErrorKind, ProjectContext, ProjectContextError};

pub type Result<T> = anyhow::Result<T>;
