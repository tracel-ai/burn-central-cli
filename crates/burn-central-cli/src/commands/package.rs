use std::path::PathBuf;
use std::sync::Arc;

use crate::commands::init::ensure_git_repo_clean;
use crate::context::CliContext;
use crate::helpers::{
    preload_functions, require_linked_project, validate_project_exists_on_server,
};
use anyhow::Context;
use burn_central_client::Client;
use burn_central_client::request::{
    BurnCentralCodeMetadataRequest, CrateVersionMetadataRequest, RegisteredFunctionRequest,
};
use burn_central_workspace::ProjectContext;
use burn_central_workspace::tools::cargo::package::{
    PackageEvent, PackagedCrateData, package_workspace,
};
use burn_central_workspace::tools::functions_registry::FunctionRegistry;
use burn_central_workspace::tools::git::is_repo_dirty;
use clap::Args;

#[derive(Args, Debug)]
pub struct PackageArgs {
    #[arg(long, action)]
    pub allow_dirty: bool,
}

pub(crate) fn handle_command(args: PackageArgs, context: CliContext) -> anyhow::Result<()> {
    let project = require_linked_project(&context)?;

    let version = package_sequence(&context, &project, None, args.allow_dirty)?;

    if version.has_uploaded {
        context
            .terminal()
            .print_success(&format!("New project version uploaded: {}", version.digest));
    } else {
        context
            .terminal()
            .print_success("No changes detected; project is up to date.");
    };

    context
        .terminal()
        .finalize("Project packaged successfully.");

    Ok(())
}

pub struct PackageResult {
    pub digest: String,
    pub has_uploaded: bool,
}

pub fn package_sequence(
    context: &CliContext,
    project: &ProjectContext,
    discovery: Option<&FunctionRegistry>,
    allow_dirty: bool,
) -> anyhow::Result<PackageResult> {
    if is_repo_dirty()? && !allow_dirty {
        ensure_git_repo_clean(context.terminal())?;
    }

    let client = context.create_client()?;

    validate_project_exists_on_server(context, project, &client)?;

    let spinner = context.terminal().spinner();
    spinner.start("Packaging workspace...");
    let spinner_clone = spinner.clone();
    let package = package_workspace(
        &project.burn_dir().artifacts_dir(),
        project.get_workspace_name(),
        Arc::new(move |msg: PackageEvent| {
            spinner_clone.set_message(msg.message);
        }),
    )
    .map_err(|e| {
        spinner.error("Packaging failed.");
        context
            .terminal()
            .print_err(&format!("Error during packaging: {}", e));
        anyhow::anyhow!("Failed to package workspace")
    })?;

    spinner.stop("Workspace packaging completed.");

    let discovery = if let Some(discovery) = discovery {
        discovery
    } else {
        &preload_functions(context, project)?
    };

    let code_metadata = BurnCentralCodeMetadataRequest {
        functions: discovery
            .get_functions()
            .into_iter()
            .map(|f| {
                let code = f.get_function_code();
                RegisteredFunctionRequest {
                    mod_path: f.mod_path,
                    fn_name: f.fn_name,
                    proc_type: f.proc_type,
                    code,
                    routine: f.routine_name,
                }
            })
            .collect(),
    };

    let bc_project = project.get_project();
    let digest = upload_new_project_version(
        &client,
        &bc_project.owner,
        &bc_project.name,
        project.get_workspace_name(),
        code_metadata,
        package.crate_metadata,
        &package.digest,
    )?;

    Ok(digest)
}

/// Upload a new version of a project to Burn Central.
pub fn upload_new_project_version(
    client: &Client,
    namespace: &str,
    project_name: &str,
    target_package_name: &str,
    code_metadata: BurnCentralCodeMetadataRequest,
    crates_data: Vec<PackagedCrateData>,
    last_commit: &str,
) -> anyhow::Result<PackageResult> {
    let (data, metadata): (Vec<(String, PathBuf)>, Vec<CrateVersionMetadataRequest>) = crates_data
        .into_iter()
        .map(|krate| {
            (
                (krate.name, krate.path),
                CrateVersionMetadataRequest {
                    checksum: krate.checksum,
                    metadata: krate.metadata,
                    size: krate.size,
                },
            )
        })
        .unzip();

    let response = client
        .publish_project_version_urls(
            namespace,
            project_name,
            target_package_name,
            code_metadata,
            metadata,
            last_commit,
        )
        .with_context(|| {
            format!("Failed to get upload URLs for project {namespace}/{project_name}")
        })?;

    if let Some(ref urls) = response.urls {
        for (crate_name, file_path) in data.into_iter() {
            let url = urls
                .get(&crate_name)
                .ok_or_else(|| anyhow::anyhow!("No upload URL found for crate: {crate_name}"))?;

            let data = std::fs::read(&file_path).map_err(|e| {
                std::io::Error::new(
                    e.kind(),
                    format!("Failed to read crate file {}: {}", file_path.display(), e),
                )
            })?;

            client
                .upload_bytes_to_url(url, data)
                .with_context(|| format!("Failed to upload crate {crate_name} to URL {url}"))?;
        }

        client
            .complete_project_version_upload(namespace, project_name, &response.id)
            .with_context(|| {
                format!("Failed to complete upload for project {namespace}/{project_name}")
            })?;
    }

    Ok(PackageResult {
        digest: response.digest,
        has_uploaded: response.urls.is_some(),
    })
}
