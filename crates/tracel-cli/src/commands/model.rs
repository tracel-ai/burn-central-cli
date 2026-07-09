use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::context::CliContext;
use crate::helpers::require_linked_project;

#[derive(Args, Debug)]
pub struct ModelArgs {
    #[command(subcommand)]
    pub command: ModelCommands,
}

#[derive(Subcommand, Debug)]
pub enum ModelCommands {
    /// Upload a local directory of files as a new model version.
    Upload(UploadModelArgs),
}

#[derive(Args, Debug)]
pub struct UploadModelArgs {
    /// Name of the model to upload a version to.
    pub model_name: String,
    /// Local directory containing the files to upload.
    pub directory: PathBuf,
    /// Burn Central namespace. Defaults to the linked project's namespace.
    #[arg(long)]
    pub namespace: Option<String>,
    /// Burn Central project name. Defaults to the linked project's name.
    #[arg(long)]
    pub project: Option<String>,
}

pub(crate) fn handle_command(args: ModelArgs, context: CliContext) -> anyhow::Result<()> {
    match args.command {
        ModelCommands::Upload(upload_args) => upload_model_version(upload_args, context),
    }
}

fn resolve_target(
    context: &CliContext,
    namespace: Option<String>,
    project: Option<String>,
) -> anyhow::Result<(String, String)> {
    if let (Some(ns), Some(proj)) = (&namespace, &project) {
        return Ok((ns.clone(), proj.clone()));
    }

    let linked = require_linked_project(context)?;
    let bc_project = linked.get_project();

    Ok((
        namespace.unwrap_or_else(|| bc_project.owner.clone()),
        project.unwrap_or_else(|| bc_project.name.clone()),
    ))
}

fn upload_model_version(args: UploadModelArgs, mut context: CliContext) -> anyhow::Result<()> {
    context.terminal().command_title("Model upload");

    let (namespace, project) = resolve_target(&context, args.namespace, args.project)?;

    context.terminal().print(&format!(
        "Target resolved: {}/{} (model '{}', directory '{}')",
        namespace,
        project,
        args.model_name,
        args.directory.display()
    ));

    context
        .terminal()
        .finalize("Target resolved. (Upload logic not yet implemented.)");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_config::Environment;
    use crate::tools::terminal::Terminal;

    #[test]
    fn given_both_namespace_and_project_when_resolve_target_then_returns_them_directly() {
        let context = CliContext::new(Terminal::default(), Environment::Production);

        let result = resolve_target(&context, Some("acme".to_string()), Some("proj".to_string()));

        assert_eq!(result.unwrap(), ("acme".to_string(), "proj".to_string()));
    }
}
