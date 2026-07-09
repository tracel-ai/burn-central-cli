use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
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

fn collect_files(directory: &Path) -> anyhow::Result<BTreeMap<String, PathBuf>> {
    let base_dir = std::fs::canonicalize(directory)
        .with_context(|| format!("Failed to resolve directory '{}'.", directory.display()))?;

    if !base_dir.is_dir() {
        anyhow::bail!("'{}' is not a directory.", base_dir.display());
    }

    let mut files = BTreeMap::new();

    for entry in walkdir::WalkDir::new(&base_dir).follow_links(false) {
        let entry =
            entry.with_context(|| format!("Failed to walk directory '{}'.", base_dir.display()))?;

        if !entry.file_type().is_file() {
            continue;
        }

        let absolute = entry.path().to_path_buf();
        let rel_path = absolute
            .strip_prefix(&base_dir)
            .expect("walked entry should be under base_dir")
            .to_string_lossy()
            .replace('\\', "/");

        files.insert(rel_path, absolute);
    }

    if files.is_empty() {
        anyhow::bail!("No files found in '{}'.", base_dir.display());
    }

    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_config::Environment;
    use crate::tools::terminal::Terminal;
    use std::fs;

    fn make_temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tracel-cli-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("nested")).unwrap();
        dir
    }

    #[test]
    fn given_both_namespace_and_project_when_resolve_target_then_returns_them_directly() {
        let context = CliContext::new(Terminal::default(), Environment::Production);

        let result = resolve_target(&context, Some("acme".to_string()), Some("proj".to_string()));

        assert_eq!(result.unwrap(), ("acme".to_string(), "proj".to_string()));
    }

    #[test]
    fn given_nested_directory_when_collect_files_then_returns_all_files_with_forward_slash_rel_paths()
     {
        let dir = make_temp_dir("collect-files");
        fs::write(dir.join("a.txt"), b"a").unwrap();
        fs::write(dir.join("nested").join("b.txt"), b"b").unwrap();

        let files = collect_files(&dir).unwrap();

        assert_eq!(files.len(), 2);
        assert!(files.contains_key("a.txt"));
        assert!(files.contains_key("nested/b.txt"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn given_empty_directory_when_collect_files_then_returns_error() {
        let dir = make_temp_dir("collect-files-empty");

        let result = collect_files(&dir);

        assert!(result.is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
