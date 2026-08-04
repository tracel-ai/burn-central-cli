use crate::commands::init::prompt_init;
use crate::commands::login::get_client_and_login_if_needed;
use crate::context::CliContext;
use crate::helpers::{is_tracel_project_linked, require_cargo_workspace};

pub mod init;
pub mod login;
pub mod me;
pub mod model;
pub mod package;
pub mod project;
pub mod training;
pub mod unlink;

/// `tracel` with no subcommand runs the project via `cargo run` (like `tracel train`
/// with no forwarded arguments), but first ensures the repository is linked to a
/// Tracel Console project, prompting for initialization if it is not.
pub fn default_command(mut context: CliContext) -> anyhow::Result<()> {
    let client = get_client_and_login_if_needed(&mut context)?;

    // Check if we have a linked Tracel Console project
    if !is_tracel_project_linked() {
        // Make sure we're at least in a Rust project before initializing
        let _crate_info = require_cargo_workspace(&context)?;
        context
            .terminal()
            .print("No Tracel Console project linked, prompting for initialization.");
        prompt_init(&context, &client)?;
        return Ok(());
    }

    training::run_cargo(&[], context)
}
