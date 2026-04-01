use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::thread;

use anyhow::Context;
use burn_central_client::Client;
use burn_central_client::request::{
    CreateModelRequest, CreateModelVersionUploadRequest, ModelVersionFileSpecRequest,
};
use burn_central_client::response::{
    ModelMultipartUploadResponse, PresignedModelFileUploadUrlsResponse,
};
use clap::{Args, Subcommand};
use crossbeam_channel::{Receiver, unbounded};
use glob::glob;
use sha2::{Digest as _, Sha256};

use crate::commands::login::get_client_and_login_if_needed;
use crate::context::CliContext;
use crate::helpers::find_manifest;

#[derive(Args, Debug)]
pub struct ModelArgs {
    #[command(subcommand)]
    pub command: ModelCommands,
}

#[derive(Subcommand, Debug)]
pub enum ModelCommands {
    /// Publish a model version from local files or glob patterns.
    Publish(PublishModelArgs),
}

#[derive(Args, Debug)]
pub struct PublishModelArgs {
    /// Model name to publish to.
    pub model_name: String,

    /// Include file(s) or glob pattern(s), repeated, relative to --base-dir.
    #[arg(long = "include", value_name = "PATH_OR_GLOB", required = true, num_args = 1..)]
    pub includes: Vec<String>,

    /// Base directory used to resolve include paths and globs.
    #[arg(long, default_value = ".", value_name = "DIR")]
    pub base_dir: PathBuf,

    /// Burn Central namespace. Defaults to the currently logged-in user namespace.
    #[arg(long, alias = "owner")]
    pub namespace: Option<String>,

    /// Burn Central project name. Defaults to the linked local project name when available.
    #[arg(long)]
    pub project: Option<String>,

    /// Number of files uploaded concurrently.
    #[arg(long, default_value_t = default_parallelism(), value_name = "N")]
    pub parallelism: usize,

    /// Description used only when creating a new model.
    #[arg(long)]
    pub description: Option<String>,

    /// Fail instead of prompting to create a missing model.
    #[arg(long, action)]
    pub no_prompt: bool,
}

struct ProjectTarget {
    namespace: String,
    project: String,
}

#[derive(Clone)]
struct FileUploadTask {
    rel_path: String,
    absolute_path: PathBuf,
    upload: ModelMultipartUploadResponse,
}

enum UploadEvent {
    PartUploaded { rel_path: String },
    FileCompleted { rel_path: String },
    FileFailed { rel_path: String, error: String },
}

fn default_parallelism() -> usize {
    thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

pub(crate) fn handle_command(args: ModelArgs, mut context: CliContext) -> anyhow::Result<()> {
    match args.command {
        ModelCommands::Publish(publish_args) => publish_model_version(publish_args, &mut context),
    }
}

fn publish_model_version(args: PublishModelArgs, context: &mut CliContext) -> anyhow::Result<()> {
    context.terminal().command_title("Model publish");

    if args.parallelism == 0 {
        anyhow::bail!("--parallelism must be greater than 0.");
    }

    let client = get_client_and_login_if_needed(context)?;
    let target = resolve_project_target(
        context,
        &client,
        args.namespace.clone(),
        args.project.clone(),
    )?;

    context.terminal().print(&format!(
        "Publishing to {}/{}",
        target.namespace, target.project
    ));

    client
        .get_project(&target.namespace, &target.project)
        .with_context(|| {
            format!(
                "Failed to resolve project {}/{} on Burn Central",
                target.namespace, target.project
            )
        })?;

    let spinner = context.terminal().spinner();
    spinner.start("Collecting files...");
    let files = collect_files(&args.base_dir, &args.includes)?;
    spinner.stop(&format!("Found {} file(s) to publish.", files.len()));

    let spinner = context.terminal().spinner();
    spinner.start("Building upload manifest...");
    let file_specs = build_file_specs(&files)?;
    spinner.stop("Upload manifest ready.");

    ensure_model_exists(context, &client, &target, &args)?;

    let spinner = context.terminal().spinner();
    spinner.start("Requesting model version upload URLs...");
    let upload = client.create_model_version_upload(
        &target.namespace,
        &target.project,
        &args.model_name,
        CreateModelVersionUploadRequest { files: file_specs },
    )?;
    spinner.stop(&format!(
        "Allocated model version {} upload.",
        upload.version
    ));

    upload_files_parallel(context, &client, &files, &upload.files, args.parallelism)?;

    client.complete_model_version_upload(
        &target.namespace,
        &target.project,
        &args.model_name,
        upload.version,
        None,
    )?;

    context.terminal().print_success(&format!(
        "Published model '{}' version {} to {}/{}.",
        args.model_name, upload.version, target.namespace, target.project
    ));
    context
        .terminal()
        .finalize("Model version published successfully.");

    Ok(())
}

fn ensure_model_exists(
    context: &CliContext,
    client: &Client,
    target: &ProjectTarget,
    args: &PublishModelArgs,
) -> anyhow::Result<()> {
    match client.get_model(&target.namespace, &target.project, &args.model_name) {
        Ok(_) => return Ok(()),
        Err(e) if e.is_not_found() => {}
        Err(e) => {
            return Err(anyhow::anyhow!(
                "Failed to check model '{}': {}",
                args.model_name,
                e
            ));
        }
    }

    if args.no_prompt {
        anyhow::bail!(
            "Model '{}' does not exist in {}/{}. Use without --no-prompt to create it.",
            args.model_name,
            target.namespace,
            target.project
        );
    }

    let create = context.terminal().confirm(&format!(
        "Model '{}' does not exist in {}/{}. Create it now?",
        args.model_name, target.namespace, target.project
    ))?;

    if !create {
        anyhow::bail!("Model publish cancelled by user");
    }

    let description = match args.description.as_ref() {
        Some(description) if !description.trim().is_empty() => Some(description.trim().to_string()),
        _ => prompt_model_description()?,
    };

    client.create_model(
        &target.namespace,
        &target.project,
        CreateModelRequest {
            name: args.model_name.clone(),
            description,
        },
    )?;

    context.terminal().print_success(&format!(
        "Created model '{}' in {}/{}.",
        args.model_name, target.namespace, target.project
    ));

    Ok(())
}

fn resolve_project_target(
    context: &CliContext,
    client: &Client,
    namespace: Option<String>,
    project: Option<String>,
) -> anyhow::Result<ProjectTarget> {
    let user = client
        .get_current_user()
        .context("Failed to resolve the logged-in user.")?;

    let namespace = namespace.unwrap_or(user.namespace);
    let project = match project {
        Some(project) => project,
        None => match try_get_linked_project_name(context) {
            Some(project) => project,
            None => {
                anyhow::bail!(
                    "No default project found. Pass --project explicitly or run from a linked Burn Central project."
                );
            }
        }
    };

    Ok(ProjectTarget { namespace, project })
}

fn try_get_linked_project_name(context: &CliContext) -> Option<String> {
    let manifest_path = find_manifest().ok()?;
    let project_context = burn_central_workspace::ProjectContext::load(
        &manifest_path,
        &context.get_burn_dir_name(),
    )
    .ok()?;

    Some(project_context.get_project().name.clone())
}

fn prompt_model_description() -> anyhow::Result<Option<String>> {
    let input = cliclack::input("Enter model description (optional)")
        .required(false)
        .interact::<String>()?;

    if input.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(input.trim().to_string()))
    }
}

fn collect_files(base_dir: &Path, includes: &[String]) -> anyhow::Result<BTreeMap<String, PathBuf>> {
    let base_dir = std::fs::canonicalize(base_dir).with_context(|| {
        format!(
            "Failed to resolve base directory '{}'.",
            base_dir.display()
        )
    })?;

    if !base_dir.is_dir() {
        anyhow::bail!("Base directory '{}' is not a directory.", base_dir.display());
    }

    let mut files = BTreeMap::new();

    for include in includes {
        let pattern = if Path::new(include).is_absolute() {
            include.clone()
        } else {
            base_dir.join(include).to_string_lossy().to_string()
        };

        let mut matched_any = false;
        for entry in glob(&pattern).with_context(|| format!("Invalid glob pattern '{}'.", include))?
        {
            let entry = entry.with_context(|| {
                format!("Failed to read a path matched by pattern '{}'.", include)
            })?;

            if !entry.is_file() {
                continue;
            }

            matched_any = true;
            let absolute = std::fs::canonicalize(&entry).with_context(|| {
                format!("Failed to resolve matched path '{}'.", entry.display())
            })?;

            if !absolute.starts_with(&base_dir) {
                anyhow::bail!(
                    "Matched file '{}' is outside base directory '{}'.",
                    absolute.display(),
                    base_dir.display()
                );
            }

            let rel = absolute
                .strip_prefix(&base_dir)
                .expect("absolute path should start with base_dir")
                .to_string_lossy()
                .replace('\\', "/");

            files.insert(rel, absolute);
        }

        if !matched_any {
            anyhow::bail!("Include pattern '{}' did not match any files.", include);
        }
    }

    if files.is_empty() {
        anyhow::bail!("No files selected for model publish.");
    }

    Ok(files)
}

fn build_file_specs(files: &BTreeMap<String, PathBuf>) -> anyhow::Result<Vec<ModelVersionFileSpecRequest>> {
    let mut specs = Vec::with_capacity(files.len());

    for (rel_path, absolute_path) in files {
        let size_bytes = std::fs::metadata(absolute_path)
            .with_context(|| format!("Failed to read file metadata '{}'.", absolute_path.display()))?
            .len();

        if size_bytes == 0 {
            anyhow::bail!(
                "File '{}' is empty. Model files must be non-empty.",
                rel_path
            );
        }

        let checksum = file_sha256(absolute_path)?;

        specs.push(ModelVersionFileSpecRequest {
            rel_path: rel_path.clone(),
            size_bytes,
            checksum,
        });
    }

    Ok(specs)
}

fn file_sha256(path: &Path) -> anyhow::Result<String> {
    let mut file = File::open(path)
        .with_context(|| format!("Failed to open file '{}'.", path.display()))?;

    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];

    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("Failed reading file '{}'.", path.display()))?;
        if read == 0 {
            break;
        }

        hasher.update(&buffer[..read]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

fn upload_file_parts_with_progress<F>(
    client: &Client,
    rel_path: &str,
    absolute_path: &Path,
    upload: &ModelMultipartUploadResponse,
    mut on_part_uploaded: F,
) -> anyhow::Result<()>
where
    F: FnMut(),
{
    let data = std::fs::read(absolute_path)
        .with_context(|| format!("Failed to read file '{}'.", absolute_path.display()))?;

    let mut parts = upload.parts.clone();
    parts.sort_by_key(|part| part.part);

    let expected_total = parts.iter().try_fold(0usize, |acc, part| {
        let size = usize::try_from(part.size_bytes).map_err(|_| {
            anyhow::anyhow!(
                "Upload chunk for '{}' exceeds addressable size on this platform.",
                rel_path
            )
        })?;
        Ok::<usize, anyhow::Error>(acc + size)
    })?;

    if expected_total != data.len() {
        anyhow::bail!(
            "Upload chunk sizes for '{}' do not match file size (expected {}, actual {}).",
            rel_path,
            expected_total,
            data.len()
        );
    }

    let mut offset = 0usize;
    for part in parts {
        let part_size = usize::try_from(part.size_bytes).map_err(|_| {
            anyhow::anyhow!(
                "Upload chunk for '{}' exceeds addressable size on this platform.",
                rel_path
            )
        })?;
        let end = offset + part_size;

        client
            .upload_bytes_to_url(&part.url, data[offset..end].to_vec())
            .with_context(|| format!("Failed to upload '{}' (part {}).", rel_path, part.part))?;

        on_part_uploaded();

        offset = end;
    }

    Ok(())
}

fn upload_files_parallel(
    context: &CliContext,
    client: &Client,
    files: &BTreeMap<String, PathBuf>,
    upload_files: &[PresignedModelFileUploadUrlsResponse],
    parallelism: usize,
) -> anyhow::Result<()> {
    let mut tasks = Vec::with_capacity(upload_files.len());
    for file in upload_files {
        let local_path = files.get(&file.rel_path).ok_or_else(|| {
            anyhow::anyhow!(
                "Upload response referenced unknown file path '{}'.",
                file.rel_path
            )
        })?;

        if file.urls.parts.is_empty() {
            anyhow::bail!(
                "Upload response for '{}' does not contain any multipart URL.",
                file.rel_path
            );
        }

        tasks.push(FileUploadTask {
            rel_path: file.rel_path.clone(),
            absolute_path: local_path.clone(),
            upload: file.urls.clone(),
        });
    }

    if tasks.is_empty() {
        anyhow::bail!("No upload tasks were generated.");
    }

    let worker_count = parallelism.min(tasks.len()).max(1);
    let multi = context.terminal().multiprogress(&format!(
        "Uploading {} file(s) with {} worker(s)",
        tasks.len(), worker_count
    ));

    let mut bars = HashMap::new();
    for task in &tasks {
        let total_parts = task.upload.parts.len() as u64;
        let bar = multi.add(cliclack::progress_bar(total_parts).with_download_template());
        bar.start(format!("Uploading {} (0/{})", task.rel_path, total_parts));
        bars.insert(task.rel_path.clone(), (bar, total_parts, 0u64));
    }

    let total_files = tasks.len();

    let (task_tx, task_rx) = unbounded::<FileUploadTask>();
    let (event_tx, event_rx) = unbounded::<UploadEvent>();

    for task in tasks {
        task_tx
            .send(task)
            .expect("Task channel should be open while enqueuing");
    }
    drop(task_tx);

    let mut handles = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let worker_client = client.clone();
        let rx = task_rx.clone();
        let tx = event_tx.clone();

        handles.push(thread::spawn(move || upload_worker(worker_client, rx, tx)));
    }
    drop(event_tx);

    let mut finished = 0usize;
    let mut failures = Vec::new();

    while finished < total_files {
        match event_rx.recv() {
            Ok(UploadEvent::PartUploaded { rel_path }) => {
                if let Some((bar, total, done)) = bars.get_mut(&rel_path) {
                    *done += 1;
                    bar.inc(1);
                    bar.set_message(format!("Uploading {} ({}/{})", rel_path, *done, *total));
                }
            }
            Ok(UploadEvent::FileCompleted { rel_path }) => {
                finished += 1;
                if let Some((bar, total, done)) = bars.get_mut(&rel_path) {
                    if *done < *total {
                        bar.inc(*total - *done);
                        *done = *total;
                    }
                    bar.stop(format!("Uploaded {} ({}/{})", rel_path, total, total));
                }
            }
            Ok(UploadEvent::FileFailed { rel_path, error }) => {
                finished += 1;
                failures.push(format!("{}: {}", rel_path, error));
                if let Some((bar, _, _)) = bars.get_mut(&rel_path) {
                    bar.error(format!("Failed {}", rel_path));
                }
            }
            Err(_) => break,
        }
    }

    for handle in handles {
        if let Err(join_err) = handle.join() {
            failures.push(format!("Upload worker crashed: {:?}", join_err));
        }
    }

    if failures.is_empty() {
        multi.stop();
        Ok(())
    } else {
        multi.error(format!("{} file(s) failed", failures.len()));
        let details = failures.join("\n");
        anyhow::bail!("Failed to upload model files:\n{}", details)
    }
}

fn upload_worker(client: Client, rx: Receiver<FileUploadTask>, tx: crossbeam_channel::Sender<UploadEvent>) {
    while let Ok(task) = rx.recv() {
        let rel_path = task.rel_path.clone();
        let result = upload_file_parts_with_progress(
            &client,
            &task.rel_path,
            &task.absolute_path,
            &task.upload,
            || {
                _ = tx.send(UploadEvent::PartUploaded {
                    rel_path: rel_path.clone(),
                });
            },
        );

        match result {
            Ok(_) => {
                _ = tx.send(UploadEvent::FileCompleted {
                    rel_path: task.rel_path,
                });
            }
            Err(err) => {
                _ = tx.send(UploadEvent::FileFailed {
                    rel_path: task.rel_path,
                    error: err.to_string(),
                });
            }
        }
    }
}
