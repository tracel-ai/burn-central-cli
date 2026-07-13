//! Project helpers for CLI operations

use crate::{
    context::CliContext,
    tools::{
        cargo::try_locate_manifest,
        project_context::{ErrorKind, ProjectContext, ProjectContextError},
        workspace::WorkspaceInfo,
    },
};
use tracel_client::Client;

/// Check if current directory contains a Rust project (has Cargo.toml)
pub fn is_cargo_workspace() -> bool {
    find_manifest().is_ok()
}

pub fn find_manifest() -> anyhow::Result<std::path::PathBuf> {
    try_locate_manifest().ok_or_else(|| {
        anyhow::anyhow!(
            "Could not locate Cargo.toml manifest. Please run this command inside a Burn project directory."
        )
    })
}

/// Check if current directory has a linked Tracel Console project
pub fn is_tracel_project_linked() -> bool {
    let manifest_path = find_manifest();
    match manifest_path {
        Err(_) => false,
        Ok(p) => ProjectContext::load(&p).is_ok(),
    }
}

pub fn handle_project_context_error(context: &CliContext, e: &ProjectContextError) {
    match e.kind() {
        ErrorKind::ManifestNotFound => {
            context
                .terminal()
                .print_err("No Cargo.toml found in current directory.");
            context
                .terminal()
                .print("Navigate to a Rust project directory first.");
        }
        ErrorKind::ProjectNotLinked => {
            context
                .terminal()
                .print_err("This Rust project is not linked to Tracel Console.");
            context
                .terminal()
                .print("Run 'tracel init' to initialize a Tracel Console project.");
        }
        ErrorKind::Parsing => {
            context.terminal().print_err(&e.to_string());
            context.terminal().print("Ensure your Cargo.toml is valid.");
        }
        ErrorKind::ProjectInitialization => {
            context.terminal().print_err(&e.to_string());
            context.terminal().print(
                "Try re-initializing the Tracel Console project with 'tracel init --force'.",
            );
        }
        ErrorKind::Unexpected => {
            context.terminal().print_err(&e.to_string());
            context
                .terminal()
                .print("An unexpected error occurred. Please check your project setup.");
        }
    }
}

/// Require a linked Tracel Console project, showing helpful errors if not found
pub fn require_linked_project(context: &CliContext) -> anyhow::Result<ProjectContext> {
    let manifest_path = find_manifest()?;
    match ProjectContext::load(&manifest_path) {
        Ok(project) => Ok(project),
        Err(e) => {
            handle_project_context_error(context, &e);
            anyhow::bail!("Failed to load linked Tracel Console project")
        }
    }
}

/// Resolve a namespace/project pair, using explicit overrides where given and
/// falling back to the linked project's namespace/name for whichever is omitted.
pub fn resolve_namespace_project(
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

/// Require a Cargo workspace (with or without Tracel Console linkage)
pub fn require_cargo_workspace(context: &CliContext) -> anyhow::Result<WorkspaceInfo> {
    let manifest_path = find_manifest()?;
    match ProjectContext::load_workspace_info(&manifest_path) {
        Ok(workspace_info) => Ok(workspace_info),
        Err(e) => {
            handle_project_context_error(context, &e);
            anyhow::bail!("Failed to load Cargo workspace info")
        }
    }
}

/// Check if we're in a valid state for initialization
pub fn can_initialize_project(context: &CliContext, force: bool) -> anyhow::Result<bool> {
    if !is_cargo_workspace() {
        context
            .terminal()
            .print_err("No Rust project found in current directory.");
        context
            .terminal()
            .print("Run this command from a Rust project directory with a Cargo.toml file.");
        return Ok(false);
    }

    if is_tracel_project_linked() {
        if force {
            return Ok(true);
        } else {
            context
                .terminal()
                .print("Project is already linked to Tracel Console.");
            context
                .terminal()
                .print("Use --force flag to reinitialize.");
            return Ok(false);
        }
    }

    Ok(true)
}

/// Validate that the linked project exists on Tracel Console server
pub fn validate_project_exists_on_server(
    context: &CliContext,
    project: &ProjectContext,
    client: &Client,
) -> anyhow::Result<()> {
    let bc_project = project.get_project();

    match client.get_project(&bc_project.owner, &bc_project.name) {
        Ok(_) => Ok(()),
        Err(e) if e.is_not_found() => {
            context.terminal().print_err(&format!(
                "Project {}/{} does not exist on Tracel Console.",
                &bc_project.owner, &bc_project.name
            ));
            context
                .terminal()
                .print("The linked project may have been deleted or renamed on the server.");
            context.terminal().print(
                "Run 'tracel init --force' to reinitialize and link to a different project.",
            );
            anyhow::bail!(
                "Project {}/{} not found on Tracel Console",
                &bc_project.owner,
                &bc_project.name
            )
        }
        Err(e) => {
            context.terminal().print_err(&format!(
                "Failed to verify project on Tracel Console: {}",
                e
            ));
            anyhow::bail!("Failed to verify project exists on server: {}", e)
        }
    }
}
