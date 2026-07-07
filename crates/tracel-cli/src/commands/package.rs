use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::Context;
use clap::Args;
use sha2::{Digest, Sha256};
use tracel_client::Client;
use tracel_client::request::{
    PublishArtifactRequest, PublishBinaryRequest, PublishProjectVersionRequest,
    PublishSourceRequest,
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

    /// Target to build for, as `<triple>[.<glibc>] [-- <raw build args>]`. Repeatable.
    /// The optional `.<glibc>` suffix (e.g. `x86_64-unknown-linux-gnu.2.28`) pins the
    /// glibc version for *-linux-gnu targets; args after ` -- ` are appended to that
    /// target's cargo build. Omit to choose targets interactively.
    #[arg(long = "target", value_name = "SPEC")]
    pub target: Vec<String>,

    /// Default glibc version applied to every selected *-linux-gnu target that has no
    /// inline `.<glibc>` suffix (routes those builds through cargo-zigbuild).
    #[arg(long, value_name = "VERSION")]
    pub glibc: Option<String>,

    /// Packaging mode. Omit to choose interactively.
    #[arg(long, value_enum, value_name = "MODE")]
    pub mode: Option<ModeArg>,

    /// Assume "yes" for toolchain-install prompts (e.g. `rustup target add`).
    #[arg(long, short = 'y', action)]
    pub yes: bool,

    /// Pick this binary by file name when a build produces several (no prompt).
    #[arg(long, value_name = "NAME")]
    pub bin: Option<String>,

    /// Raw build args applied to every target (everything after a top-level `--`).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub global_args: Vec<String>,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
#[value(rename_all = "lower")]
pub enum ModeArg {
    Binary,
    Source,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Binary,
    Source,
}

impl From<ModeArg> for Mode {
    fn from(m: ModeArg) -> Self {
        match m {
            ModeArg::Binary => Mode::Binary,
            ModeArg::Source => Mode::Source,
        }
    }
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

    // 3. Choose how to package. Binary-only flags imply binary mode when `--mode` is
    // omitted; passing them with `--mode source` is a conflict.
    let binary_flags = !args.target.is_empty() || args.glibc.is_some();
    let mode = match args.mode {
        Some(m) => Mode::from(m),
        None if binary_flags => Mode::Binary,
        None => cliclack::select("How would you like to package your code?")
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
            .interact()?,
    };
    if mode == Mode::Source && binary_flags {
        anyhow::bail!("`--target`/`--glibc` only apply to binary packaging (`--mode binary`).");
    }

    let artifact = match mode {
        Mode::Source => build_source_artifact(&context, &project)?,
        Mode::Binary => build_binary_artifact(&context, &project, &args)?,
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
    args: &PackageArgs,
) -> anyhow::Result<PreparedArtifact> {
    let host = target::host_target()?;
    let installed = target::installed_targets();

    // Targets come from `--target` when given, otherwise the interactive multiselect.
    // Either way we end up with fully-resolved `TargetSpec`s (glibc + raw args applied).
    let selected: Vec<target::TargetSpec> = if !args.target.is_empty() {
        target::resolve_target_specs(&args.target, args.glibc.as_deref(), &args.global_args)?
    } else {
        let mut specs: Vec<target::TargetSpec> = target::prompt_targets(host, &installed)?
            .into_iter()
            .map(|(os, arch)| target::TargetSpec {
                os,
                arch,
                glibc: None,
                raw_args: args.global_args.clone(),
            })
            .collect();
        target::apply_glibc_default(&mut specs, args.glibc.as_deref())?;
        specs
    };

    // rustup preflight: install any missing *base* std. A glibc pin still builds against
    // the base triple's std, and a host build with a glibc pin now passes `--target` too,
    // so both need the base target installed.
    let missing: Vec<&str> = selected
        .iter()
        .filter(|s| (s.os, s.arch) != host || s.glibc.is_some())
        .map(|s| target::target_triple(s.os, s.arch))
        .filter(|triple| !installed.contains(*triple))
        .collect();
    target::install_missing_target(missing, args.yes)?;

    let root = project.get_workspace_root();
    let drivers = build_driver::detect();
    let mut binaries = Vec::new();
    let mut uploads = Vec::new();

    for spec in selected {
        let (os, arch) = (spec.os, spec.arch);
        let base = target::target_triple(os, arch); // canonical triple — the upload key
        let is_host = (os, arch) == host;
        let needs_glibc = spec.glibc.is_some();

        // A glibc pin forces zigbuild (the only driver that honours the suffix), even
        // for a host or same-OS build that would otherwise use plain `cargo build`.
        let driver = if needs_glibc {
            BuildDriver::Zigbuild
        } else if is_host {
            BuildDriver::Cargo
        } else {
            build_driver::choose(host, (os, arch), &drivers)
        };

        let linker = if needs_glibc {
            build_driver::glibc_preflight(
                context.terminal(),
                &drivers,
                (os, arch),
                spec.glibc.as_deref().unwrap(),
            )?;
            None // zig supplies the linker; never inject a cargo linker entry
        } else if is_host {
            context.terminal().print_warning(&format!(
                "Building for this machine ({base}). It will only run on compute providers with the same OS and architecture."
            ));
            None
        } else {
            build_driver::cross_preflight(context.terminal(), root, host, (os, arch), driver)?
        };

        // The glibc suffix is appended only to the `--target` value passed to zig; the
        // (os, arch) and upload key stay canonical. A plain host build passes no `--target`.
        let target_arg: Option<String> = match &spec.glibc {
            Some(glibc) => Some(format!("{base}.{glibc}")),
            None if is_host => None,
            None => Some(base.to_string()),
        };

        let path = build_release_binary(
            context,
            target_arg.as_deref(),
            driver,
            linker,
            &spec.raw_args,
            args.bin.as_deref(),
        )?;
        let (checksum, size) = sha256_and_size(&path)?;
        binaries.push(PublishBinaryRequest {
            os,
            architecture: arch,
            checksum,
            size,
        });
        uploads.push((base.to_string(), path));
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
    extra_args: &[String],
    bin: Option<&str>,
) -> anyhow::Result<PathBuf> {
    let mut cmd_label = match target {
        Some(triple) => format!("{} --release --target {triple}", driver.label()),
        None => format!("{} --release", driver.label()),
    };
    if let Some(linker) = linker {
        cmd_label.push_str(&format!(" (linker {linker})"));
    }
    if !extra_args.is_empty() {
        cmd_label.push(' ');
        cmd_label.push_str(&extra_args.join(" "));
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
    // Raw per-target/global args are appended verbatim, so the user can steer the build.
    command.args(extra_args);
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

    let binary_name = |p: &Path| -> String {
        p.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.display().to_string())
    };

    match executables.len() {
        0 => anyhow::bail!("The build did not produce any binary target."),
        1 => Ok(executables.into_iter().next().unwrap()),
        _ => {
            let names = || {
                executables
                    .iter()
                    .map(|p| binary_name(p))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            // `--bin <name>` picks deterministically; otherwise prompt only when there is
            // an interactive terminal, else bail so scripts don't hang.
            if let Some(want) = bin {
                let mut matched = executables.iter().filter(|p| binary_name(p) == want);
                match (matched.next(), matched.next()) {
                    (Some(p), None) => Ok(p.clone()),
                    (None, _) => {
                        anyhow::bail!("No built binary named `{want}`. Built: {}.", names())
                    }
                    (Some(_), Some(_)) => {
                        anyhow::bail!("`--bin {want}` is ambiguous among: {}.", names())
                    }
                }
            } else if !std::io::stdin().is_terminal() {
                anyhow::bail!(
                    "The build produced multiple binaries ({}). Pass `--bin <name>` to choose one.",
                    names()
                )
            } else {
                let items: Vec<(PathBuf, String, &str)> = executables
                    .iter()
                    .map(|p| (p.clone(), binary_name(p), ""))
                    .collect();
                cliclack::select("Multiple binaries were built. Select which to upload")
                    .items(&items)
                    .interact()
                    .map_err(anyhow::Error::from)
            }
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

    let Some(urls) = response.urls else {
        context.terminal().print_success(&format!(
            "This commit ({digest}) is already packaged (version {}).",
            response.id
        ));
        context.terminal().finalize("Nothing to upload.");
        return Ok(());
    };

    let spinner = context.terminal().spinner();
    spinner.start("Uploading artifacts...");
    for (key, path) in prepared.uploads {
        let url = urls.get(&key).ok_or_else(|| {
            spinner.error("Upload failed.");
            anyhow::anyhow!("Server did not return an upload URL for `{key}`")
        })?;
        let bytes =
            std::fs::read(&path).with_context(|| format!("Failed to read {}", path.display()))?;
        client.upload_bytes_to_url(url, bytes).map_err(|e| {
            spinner.error("Upload failed.");
            anyhow::anyhow!("Failed to upload `{key}`: {e}")
        })?;
    }
    spinner.stop("Artifacts uploaded.");

    client
        .complete_project_version_upload(&bc_project.owner, &bc_project.name, &response.id)
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Thin wrapper so we can parse `PackageArgs` from an argv in tests.
    #[derive(Parser, Debug)]
    struct Wrap {
        #[command(flatten)]
        args: PackageArgs,
    }

    fn parse(argv: &[&str]) -> PackageArgs {
        Wrap::try_parse_from(argv).unwrap().args
    }

    #[test]
    fn repeatable_target_and_flags() {
        let a = parse(&[
            "package", "--target", "x86_64-unknown-linux-gnu", "--target",
            "aarch64-unknown-linux-gnu.2.17", "-y", "--mode", "binary",
        ]);
        assert_eq!(
            a.target,
            vec!["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu.2.17"]
        );
        assert!(a.yes);
        assert_eq!(a.mode, Some(ModeArg::Binary));
        assert!(a.global_args.is_empty());
    }

    #[test]
    fn trailing_dashdash_feeds_global_args() {
        // `--target` before `--` parses normally; everything after `--` is global args.
        let a = parse(&[
            "package", "--target", "x86_64-unknown-linux-gnu", "--", "--config",
            "net.git-fetch-with-cli=true",
        ]);
        assert_eq!(a.target, vec!["x86_64-unknown-linux-gnu"]);
        assert_eq!(a.global_args, vec!["--config", "net.git-fetch-with-cli=true"]);
    }

    #[test]
    fn mode_source_lowercase() {
        assert_eq!(parse(&["package", "--mode", "source"]).mode, Some(ModeArg::Source));
    }
}
