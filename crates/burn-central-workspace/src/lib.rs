pub mod compute_provider;
mod entity;
pub mod event;
pub mod execution;
mod generation;
pub mod logging;
pub mod tools;

pub use entity::projects::burn_dir::project::BurnCentralProject;
pub use entity::projects::{ErrorKind, ProjectContext, ProjectContextError, WorkspaceInfo};

pub type Result<T> = anyhow::Result<T>;
