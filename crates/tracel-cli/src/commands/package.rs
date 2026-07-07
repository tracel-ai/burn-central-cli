use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::Context;
use clap::Args;
use sha2::{Digest, Sha256};
use tracel_client::Client;
use tracel_client::request::{
    CompleteCodeUploadRequest, CompletedCodeArtifact, CompletedCodePart, PublishArtifactRequest,
    PublishBinaryRequest, PublishProjectVersionRequest, PublishSourceRequest,
};

use crate::commands::init::commit_sequence;
use crate::commands::login::get_client_and_login_if_needed;
use crate::context::CliContext;
use crate::helpers::{require_linked_project, validate_project_exists_on_server};
use crate::tools::build_driver::{self, BuildDriver};
use crate::tools::packager::{PackageEvent, package_workspace};
use crate::tools::project_context::ProjectContext;
use crate::tools::{cargo, git, target};

#[derive(Args, Debug)]
pub struct PackageArgs {
    /// Package even if the git repository has uncommitted changes (skips the commit prompt).
    #[arg(long, action)]
    pub allow_dirty: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Binary,
    Source,
}

/// An artifact prepared for upload: the publish request describing it, plus the
/// `(upload-url key, file path)` pairs whose bytes must be PUT to the presigned
/// URLs the server returns.
struct PreparedArtifact {
    request: PublishArtifactRequest,
    uploads: Vec<(String, PathBuf)>,
}

pub(crate) fn handle_command(args: PackageArgs, mut context: CliContext) -> anyhow::Result<()> {
    context.terminal().command_title("Package project");

    // 0. Ensure we have auth and a linked project that exists on the server.
    let client = get_client_and_login_if_needed(&mut context)?;
    let project = require_linked_project(&context)?;
    validate_project_exists_on_server(&context, &project, &client)?;

    // 1. Dirty check — warn and offer to commit, but allow proceeding.
    if git::is_repo_dirty()? && !args.allow_dirty {
        context
            .terminal()
            .print_warning("Your repository has uncommitted changes.");
        if cliclack::confirm("Commit changes before packaging?")
            .initial_value(true)
            .interact()?
        {
            commit_sequence()?;
        }
    }

    // 2. The code version is identified by the current commit hash.
    let digest = git::get_last_commit_hash().context(
        "Failed to read the current git commit. The repository needs at least one commit to package.",
    )?;
    if git::is_repo_dirty()? {
        context.terminal().print_warning(&format!(
            "Proceeding with uncommitted changes — they will not be part of code version {digest}."
        ));
    }

    // 3. Choose how to package.
    let mode = cliclack::select("How would you like to package your code?")
        .items(&[
            (
                Mode::Binary,
                "Binary (more secure)",
                "ship a compiled binary; your source is not uploaded",
            ),
            (
                Mode::Source,
                "Source (more portable)",
                "upload source; it is built on the compute provider",
            ),
        ])
        .interact()?;

    let artifact = match mode {
        Mode::Source => build_source_artifact(&context, &project)?,
        Mode::Binary => build_binary_artifact(&context, &project)?,
    };

    // 4. Upload.
    upload(&context, &client, &project, &digest, artifact)
}

fn build_source_artifact(
    context: &CliContext,
    project: &ProjectContext,
) -> anyhow::Result<PreparedArtifact> {
    let spinner = context.terminal().spinner();
    spinner.start("Packaging workspace...");
    let spinner_clone = spinner.clone();
    let result = package_workspace(
        project.get_workspace_name(),
        Arc::new(move |msg: PackageEvent| {
            spinner_clone.set_message(msg.message);
        }),
    )
    .map_err(|e| {
        spinner.error("Packaging failed.");
        anyhow::anyhow!("Failed to package workspace: {e}")
    })?;
    spinner.stop("Workspace packaged.");

    Ok(PreparedArtifact {
        request: PublishArtifactRequest::Source {
            source: PublishSourceRequest {
                checksum: result.checksum,
                size: result.size,
            },
        },
        uploads: vec![("source.zip".to_string(), result.path)],
    })
}

fn build_binary_artifact(
    context: &CliContext,
    project: &ProjectContext,
) -> anyhow::Result<PreparedArtifact> {
    let host = target::host_target()?;
    let installed = target::installed_targets();

    let selected = target::prompt_targets(host, &installed)?;

    // rustup preflight: offer to install any selected cross target whose std is missing.
    let missing: Vec<&str> = selected
        .iter()
        .filter(|&&(os, arch)| (os, arch) != host)
        .map(|&(os, arch)| target::target_triple(os, arch))
        .filter(|triple| !installed.contains(*triple))
        .collect();
    target::install_missing_target(missing)?;

    let root = project.get_workspace_root();
    let drivers = build_driver::detect();
    let mut binaries = Vec::new();
    let mut uploads = Vec::new();

    for (os, arch) in selected {
        let triple = target::target_triple(os, arch);
        let is_host = (os, arch) == host;
        let driver = if is_host {
            BuildDriver::Cargo
        } else {
            build_driver::choose(host, (os, arch), &drivers)
        };

        let linker = if is_host {
            context.terminal().print_warning(&format!(
                "Building for this machine ({triple}). It will only run on compute providers with the same OS and architecture."
            ));
            None
        } else {
            build_driver::cross_preflight(context.terminal(), root, host, (os, arch), driver)?
        };

        let path = build_release_binary(context, (!is_host).then_some(triple), driver, linker)?;
        let (checksum, size) = sha256_and_size(&path)?;
        binaries.push(PublishBinaryRequest {
            os,
            architecture: arch,
            checksum,
            size,
        });
        uploads.push((triple.to_string(), path));
    }

    Ok(PreparedArtifact {
        request: PublishArtifactRequest::Binaries { binaries },
        uploads,
    })
}

/// Run the release build with `driver` (optionally for a cross `--target`) and return
/// the path to the produced executable (prompting if the build produced more than one).
fn build_release_binary(
    context: &CliContext,
    target: Option<&str>,
    driver: BuildDriver,
    linker: Option<&str>,
) -> anyhow::Result<PathBuf> {
    let mut cmd_label = match target {
        Some(triple) => format!("{} --release --target {triple}", driver.label()),
        None => format!("{} --release", driver.label()),
    };
    if let Some(linker) = linker {
        cmd_label.push_str(&format!(" (linker {linker})"));
    }
    context
        .terminal()
        .print(&format!("Building release binary ({cmd_label})..."));

    let mut command = cargo::command();
    for arg in driver.subcommand_args() {
        command.arg(arg);
    }
    command.arg("--release").arg("--message-format=json");
    if let Some(triple) = target {
        command.arg("--target").arg(triple);
        if let Some(linker) = linker {
            command
                .arg("--config")
                .arg(format!("target.{triple}.linker=\"{linker}\""));
        }
    }
    let output = command
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("Failed to run `{cmd_label}`"))?;

    if !output.status.success() {
        anyhow::bail!("`{cmd_label}` failed");
    }

    let mut executables: Vec<PathBuf> = Vec::new();
    for line in output.stdout.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        if let Ok(msg) = serde_json::from_slice::<serde_json::Value>(line) {
            if msg.get("reason").and_then(|r| r.as_str()) == Some("compiler-artifact") {
                if let Some(exe) = msg.get("executable").and_then(|e| e.as_str()) {
                    executables.push(PathBuf::from(exe));
                }
            }
        }
    }

    match executables.len() {
        0 => anyhow::bail!("The build did not produce any binary target."),
        1 => Ok(executables.into_iter().next().unwrap()),
        _ => {
            let items: Vec<(PathBuf, String, &str)> = executables
                .iter()
                .map(|p| {
                    (
                        p.clone(),
                        p.file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| p.display().to_string()),
                        "",
                    )
                })
                .collect();
            cliclack::select("Multiple binaries were built. Select which to upload")
                .items(&items)
                .interact()
                .map_err(anyhow::Error::from)
        }
    }
}

fn sha256_and_size(path: &Path) -> anyhow::Result<(String, u64)> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("Failed to read binary at {}", path.display()))?;
    let checksum = format!("{:x}", Sha256::digest(&bytes));
    Ok((checksum, bytes.len() as u64))
}

fn upload(
    context: &CliContext,
    client: &Client,
    project: &ProjectContext,
    digest: &str,
    prepared: PreparedArtifact,
) -> anyhow::Result<()> {
    let bc_project = project.get_project();

    let response = client
        .publish_project_version_urls(
            &bc_project.owner,
            &bc_project.name,
            PublishProjectVersionRequest {
                digest: digest.to_string(),
                artifact: prepared.request,
            },
        )
        .with_context(|| {
            format!(
                "Failed to request upload URLs for {}/{}",
                bc_project.owner, bc_project.name
            )
        })?;

    // No uploads returned means this commit is already fully packaged.
    if response.uploads.is_empty() {
        context.terminal().print_success(&format!(
            "This commit ({digest}) is already packaged (version {}).",
            response.id
        ));
        context.terminal().finalize("Nothing to upload.");
        return Ok(());
    }

    let spinner = context.terminal().spinner();
    spinner.start("Uploading artifacts...");

    // PUT each artifact, capture the S3 `ETag`, and report the exact `upload_id`
    // + parts back so the server finalizes the uploads we actually wrote to
    // (robust against server-side cache drift).
    let mut artifacts = Vec::new();
    for (key, path) in prepared.uploads {
        let descriptor = response.uploads.get(&key).ok_or_else(|| {
            spinner.error("Upload failed.");
            anyhow::anyhow!("Server did not return an upload URL for `{key}`")
        })?;
        let part = descriptor.parts.first().ok_or_else(|| {
            spinner.error("Upload failed.");
            anyhow::anyhow!("Server returned no upload part for `{key}`")
        })?;
        let bytes =
            std::fs::read(&path).with_context(|| format!("Failed to read {}", path.display()))?;
        let etag = client
            .upload_bytes_to_url(&part.url, bytes)
            .map_err(|e| {
                spinner.error("Upload failed.");
                anyhow::anyhow!("Failed to upload `{key}`: {e}")
            })?
            .ok_or_else(|| {
                spinner.error("Upload failed.");
                anyhow::anyhow!("Upload of `{key}` did not return an ETag")
            })?;
        artifacts.push(CompletedCodeArtifact {
            key,
            upload_id: descriptor.id.clone(),
            parts: vec![CompletedCodePart {
                part_number: part.part as i32,
                etag,
            }],
        });
    }
    spinner.stop("Artifacts uploaded.");

    client
        .complete_project_version_upload(
            &bc_project.owner,
            &bc_project.name,
            &response.id,
            CompleteCodeUploadRequest { artifacts },
        )
        .with_context(|| {
            format!(
                "Failed to finalize upload for {}/{}",
                bc_project.owner, bc_project.name
            )
        })?;

    context
        .terminal()
        .print_success(&format!("New code version uploaded: {}", response.digest));
    context
        .terminal()
        .finalize("Project packaged successfully.");
    Ok(())
}
