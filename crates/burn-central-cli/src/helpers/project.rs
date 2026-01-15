//! Project helpers for CLI operations

use crate::context::CliContext;
use anyhow::Context;
use burn_central_client::Client;
use burn_central_workspace::{
    CrateInfo, ProjectContext,
    tools::{cargo, function_discovery::DiscoveryEvent, functions_registry::FunctionRegistry},
};
use colored::Colorize;
use std::sync::Arc;

/// Check if current directory contains a Rust project (has Cargo.toml)
pub fn is_rust_project() -> bool {
    find_manifest().is_ok()
}

pub fn find_manifest() -> anyhow::Result<std::path::PathBuf> {
    cargo::try_locate_manifest().ok_or_else(|| {
        anyhow::anyhow!(
            "Could not locate Cargo.toml manifest. Please run this command inside a Burn project directory."
        )
    })
}

/// Check if current directory has a linked Burn Central project
pub fn is_burn_central_project_linked(context: &CliContext) -> bool {
    let manifest_path = find_manifest();
    match manifest_path {
        Err(_) => false,
        Ok(p) => ProjectContext::load(&p, &context.get_burn_dir_name()).is_ok(),
    }
}

pub fn handle_project_context_error(
    context: &CliContext,
    e: &burn_central_workspace::ProjectContextError,
) {
    match e.kind() {
        burn_central_workspace::ErrorKind::ManifestNotFound => {
            context
                .terminal()
                .print_err("No Cargo.toml found in current directory.");
            context
                .terminal()
                .print("Navigate to a Rust project directory first.");
        }
        burn_central_workspace::ErrorKind::BurnDirNotInitialized => {
            context
                .terminal()
                .print_err("This Rust project is not linked to Burn Central.");
            context
                .terminal()
                .print("Run 'burn init' to initialize a Burn Central project.");
        }
        burn_central_workspace::ErrorKind::Parsing => {
            context.terminal().print_err(&e.to_string());
            context.terminal().print("Ensure your Cargo.toml is valid.");
        }
        burn_central_workspace::ErrorKind::InvalidPackage => {
            context.terminal().print_err(&e.to_string());
            context
                .terminal()
                .print("Ensure your Cargo.toml defines a valid Rust package.");
        }
        burn_central_workspace::ErrorKind::BurnDirInitialization => {
            context.terminal().print_err(&e.to_string());
            context
                .terminal()
                .print("Try re-initializing the Burn Central project with 'burn init --force'.");
        }
        burn_central_workspace::ErrorKind::Unexpected => {
            context.terminal().print_err(&e.to_string());
            context
                .terminal()
                .print("An unexpected error occurred. Please check your project setup.");
        }
    }
}

/// Require a linked Burn Central project, showing helpful errors if not found
pub fn require_linked_project(context: &CliContext) -> anyhow::Result<ProjectContext> {
    let manifest_path = find_manifest()?;
    match ProjectContext::load(&manifest_path, &context.get_burn_dir_name()) {
        Ok(project) => Ok(project),
        Err(e) => {
            handle_project_context_error(context, &e);
            anyhow::bail!("Failed to load linked Burn Central project")
        }
    }
}

/// Require a Rust project (with or without Burn Central linkage)
pub fn require_rust_project(context: &CliContext) -> anyhow::Result<CrateInfo> {
    let manifest_path = find_manifest()?;
    match ProjectContext::load_crate_info(&manifest_path) {
        Ok(crate_info) => Ok(crate_info),
        Err(e) => {
            handle_project_context_error(context, &e);
            anyhow::bail!("Failed to load Rust project crate info")
        }
    }
}

/// Check if we're in a valid state for initialization
pub fn can_initialize_project(context: &CliContext, force: bool) -> anyhow::Result<bool> {
    if !is_rust_project() {
        context
            .terminal()
            .print_err("No Rust project found in current directory.");
        context
            .terminal()
            .print("Run this command from a Rust project directory with a Cargo.toml file.");
        return Ok(false);
    }

    if is_burn_central_project_linked(context) {
        if force {
            return Ok(true);
        } else {
            context
                .terminal()
                .print("Project is already linked to Burn Central.");
            context
                .terminal()
                .print("Use --force flag to reinitialize.");
            return Ok(false);
        }
    }

    Ok(true)
}

/// Validate that the linked project exists on Burn Central server
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
                "Project {}/{} does not exist on Burn Central.",
                &bc_project.owner, &bc_project.name
            ));
            context
                .terminal()
                .print("The linked project may have been deleted or renamed on the server.");
            context
                .terminal()
                .print("Run 'burn init --force' to reinitialize and link to a different project.");
            anyhow::bail!(
                "Project {}/{} not found on Burn Central",
                &bc_project.owner,
                &bc_project.name
            )
        }
        Err(e) => {
            context
                .terminal()
                .print_err(&format!("Failed to verify project on Burn Central: {}", e));
            anyhow::bail!("Failed to verify project exists on server: {}", e)
        }
    }
}

pub fn preload_functions(
    context: &CliContext,
    project: &ProjectContext,
) -> anyhow::Result<FunctionRegistry> {
    let spinner = context.terminal().spinner();
    spinner.start("Discovering project functions...");

    let spinner_clone = spinner.clone();
    let functions =
        project
            .load_functions(Some(Arc::new(move |event: DiscoveryEvent| {
                let mut message = format!(
                    "Discovering project functions: Checking {}",
                    event.package.name.bold()
                );
                if let Some(msg) = event.message {
                    message = format!("{} - {}", message, msg);
                }
                spinner_clone.set_message(message);
            })))
            .inspect_err(|e| {
                spinner.error("Failed to discover project functions.");
                match e {
                burn_central_workspace::tools::function_discovery::DiscoveryError::CargoError {
                    package: _,
                    status: _,
                    diagnostics,
                } => {
                    context.terminal().print_err(&format!("Error: {}", e));

                    let header = "=== RUSTC DIAGNOSTICS ===";
                    let footer = "=".repeat(header.len());
                    let message =
                        format!("{}\n\n{}\n{}", header.yellow(), diagnostics, footer.yellow());
                    context.terminal().print_err(&message);
                }
                _ => {
                    context.terminal().print_err(&format!("Error: {}", e));
                }
            }
            })
            .context("Function discovery failed")?;

    spinner.stop(format!(
        "Discovered {} functions.",
        functions.get_function_references().len()
    ));
    Ok(functions)
}
