use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::context::CliContext;
use crate::helpers::{
    build_file_specs, build_part_tasks, collect_files, ensure_model_exists, require_linked_project,
    upload_parts,
};

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
    /// Tracel Console namespace. Defaults to the linked project's namespace.
    #[arg(long)]
    pub namespace: Option<String>,
    /// Tracel Console project name. Defaults to the linked project's name.
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

    let client = crate::commands::login::get_client_and_login_if_needed(&mut context)?;
    let (namespace, project) = resolve_target(&context, args.namespace, args.project)?;

    context
        .terminal()
        .print(&format!("Uploading to {namespace}/{project}"));

    let spinner = context.terminal().spinner();
    spinner.start("Collecting files...");
    let files = collect_files(&args.directory).inspect_err(|_e| {
        spinner.error("Failed to collect files.");
    })?;
    spinner.stop(format!("Found {} file(s).", files.len()));

    ensure_model_exists(&context, &client, &namespace, &project, &args.model_name)?;

    let spinner = context.terminal().spinner();
    spinner.start("Computing checksums...");
    let file_specs = build_file_specs(&files).inspect_err(|_e| {
        spinner.error("Failed to compute checksums.");
    })?;
    spinner.stop("Checksums computed.");

    let file_sizes: std::collections::BTreeMap<String, u64> = file_specs
        .iter()
        .map(|f| (f.rel_path.clone(), f.size_bytes))
        .collect();

    let spinner = context.terminal().spinner();
    spinner.start("Requesting upload URLs...");
    let upload_request = tracel_client::request::RequestModelVersionUploadRequest {
        files: file_specs
            .into_iter()
            .map(|f| tracel_client::request::ModelFileSpecRequest {
                rel_path: f.rel_path,
                size_bytes: f.size_bytes,
                checksum: f.checksum,
            })
            .collect(),
    };
    let upload = client
        .request_model_version_upload(&namespace, &project, &args.model_name, upload_request)
        .map_err(|e| {
            spinner.error("Failed to request upload URLs.");
            anyhow::anyhow!(e)
        })?;
    spinner.stop(format!("Allocated model version {}.", upload.version));

    let tasks = build_part_tasks(&files, &file_sizes, &upload.files)?;
    upload_parts(&client, tasks)?;

    client.complete_model_version_upload(&namespace, &project, &args.model_name, upload.version)?;

    context.terminal().print_success(&format!(
        "Uploaded model '{}' version {} to {}/{}.",
        args.model_name, upload.version, namespace, project
    ));
    context
        .terminal()
        .finalize("Model version uploaded successfully.");

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
