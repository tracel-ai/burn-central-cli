//! Workspace packaging functionality for Burn Central
//!
//! This module provides functionality to package an entire workspace as a single compressed archive,
//! respecting gitignore rules. This is used to upload workspace projects to Burn Central while
//! maintaining backwards compatibility with the single-crate API.

use std::{
    collections::BTreeMap,
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
};

use colored::Colorize;
use sha2::Digest as _;
use sha2::Sha256;

use crate::print_info;

use super::schemas::{CrateMetadata, Dep};

#[derive(Debug)]
pub struct PackagedCrateData {
    pub name: String,
    pub path: PathBuf,
    pub checksum: String,
    pub metadata: serde_json::Value,
    pub size: u64,
}

#[derive(Debug)]
pub struct PackageResult {
    pub crate_metadata: Vec<PackagedCrateData>,
    pub digest: String,
}

pub struct PackageEvent {
    pub message: String,
}

type PackageEventReporter = dyn crate::event::Reporter<PackageEvent>;

/// Package the entire workspace as a single compressed archive with gitignore applied.
///
/// Returns a `PackageResult` containing the packaged workspace data and digest
pub fn package_workspace(
    artifacts_dir: &Path,
    workspace_name: &str,
    event_reporter: Arc<PackageEventReporter>,
) -> anyhow::Result<PackageResult> {
    event_reporter.report_event(PackageEvent {
        message: "Initializing workspace packaging".to_string(),
    });

    // Get workspace metadata
    let cmd = cargo_metadata::MetadataCommand::new();
    let metadata = cmd.exec()?;

    let workspace_root = metadata
        .workspace_root
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("Failed to canonicalize workspace root: {}", e))?;

    print_info!(
        "Packaging workspace at: {}",
        workspace_root.display().to_string().bold()
    );

    // List all files in the workspace respecting gitignore
    event_reporter.report_event(PackageEvent {
        message: "Discovering files (respecting .gitignore)".to_string(),
    });

    let files = list_workspace_files(&workspace_root)?;

    print_info!("Found {} files to package", files.len());

    event_reporter.report_event(PackageEvent {
        message: format!("Discovered {} files", files.len()),
    });

    // Create the archive
    event_reporter.report_event(PackageEvent {
        message: "Creating compressed archive".to_string(),
    });

    // Use standard cargo crate naming format to match backend expectations
    // The backend returns upload URLs keyed by {name}-{version}.crate
    const WORKSPACE_VERSION: &str = "0.1.0";
    let archive_name = format!("{}-{}.crate", workspace_name, WORKSPACE_VERSION);
    let archive_path = artifacts_dir.join(&archive_name);

    std::fs::create_dir_all(artifacts_dir)?;

    let archive_file = File::create(&archive_path)?;
    let package_prefix = format!("{}-{}", workspace_name, WORKSPACE_VERSION);
    let uncompressed_size =
        create_workspace_archive(&workspace_root, &files, &archive_file, &package_prefix)?;

    event_reporter.report_event(PackageEvent {
        message: format!(
            "Archive created: {}",
            human_readable_bytes(uncompressed_size)
        ),
    });

    // Calculate checksum
    event_reporter.report_event(PackageEvent {
        message: "Computing checksum".to_string(),
    });

    let archive_data = std::fs::read(&archive_path)?;
    let checksum = format!("{:x}", Sha256::digest(&archive_data));
    let size = archive_data.len() as u64;

    // Build metadata from cargo_metadata
    event_reporter.report_event(PackageEvent {
        message: "Extracting package metadata".to_string(),
    });

    // let crate_metadata = build_crate_metadata(primary_package, &metadata)?;
    let fake_metadata = CrateMetadata::new(
        workspace_name.to_string(),
        WORKSPACE_VERSION.to_string(),
        Vec::new(),
        BTreeMap::new(),
        Vec::new(),
        Some("Workspace package".to_string()),
        None,
        None,
        None,
        None,
        Vec::new(),
        Vec::new(),
        None,
        None,
        None,
        BTreeMap::new(),
        None,
    );

    let packaged_data = PackagedCrateData {
        name: archive_name,
        path: archive_path,
        checksum: checksum.clone(),
        metadata: serde_json::to_value(&fake_metadata)?,
        size,
    };

    // Calculate final digest
    let digest = checksum; // For a single package, the digest is the same as the checksum

    event_reporter.report_event(PackageEvent {
        message: "Workspace packaging completed successfully".to_string(),
    });

    Ok(PackageResult {
        crate_metadata: vec![packaged_data],
        digest,
    })
}

/// Lists all files in the workspace that should be packaged, respecting gitignore rules.
fn list_workspace_files(workspace_root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let git_repo = discover_gix_repo(workspace_root)?;

    if let Some(ref repo) = git_repo {
        print_info!(
            "Git repository found at {}",
            repo.path().display().to_string().bold()
        );
    }

    // Build gitignore matcher
    let mut exclude_builder = ignore::gitignore::GitignoreBuilder::new(workspace_root);

    // Add default excludes if not using git
    if git_repo.is_none() {
        exclude_builder.add_line(None, ".*")?;
    }

    // Add common excludes
    exclude_builder.add_line(None, "target/")?;
    exclude_builder.add_line(None, ".burn/")?;

    let ignore_exclude = exclude_builder.build()?;

    let filter = |path: &Path, is_dir: bool| {
        let Ok(relative_path) = path.strip_prefix(workspace_root) else {
            return false;
        };

        // Always exclude target and .burn directories
        if let Some(first_component) = relative_path.components().next() {
            let component_str = first_component.as_os_str().to_string_lossy();
            if component_str == "target" || component_str == ".burn" {
                return false;
            }
        }

        // Check gitignore rules
        !ignore_exclude
            .matched_path_or_any_parents(relative_path, is_dir)
            .is_ignore()
    };

    // Use git if available, otherwise walk the filesystem
    if let Some(repo) = git_repo {
        list_files_gix(workspace_root, &repo, &filter)
    } else {
        let mut files = Vec::new();
        list_files_walk(workspace_root, &mut files, true, &filter)?;
        Ok(files)
    }
}

/// Creates a compressed tar.gz archive of the workspace files.
///
/// All files are prefixed with `{package_prefix}/` to match the standard cargo crate format.
fn create_workspace_archive(
    workspace_root: &Path,
    files: &[PathBuf],
    dst: &File,
    package_prefix: &str,
) -> anyhow::Result<u64> {
    let encoder = flate2::GzBuilder::new().write(dst, flate2::Compression::best());

    let mut ar = tar::Builder::new(encoder);
    let mut uncompressed_size: u64 = 0;

    for file_path in files {
        let relative_path = file_path
            .strip_prefix(workspace_root)
            .map_err(|e| anyhow::anyhow!("Failed to strip workspace root prefix: {}", e))?;

        if file_path.is_file() {
            let mut file = File::open(file_path)?;
            let metadata = file.metadata()?;

            let mut header = tar::Header::new_gnu();
            header.set_metadata_in_mode(&metadata, tar::HeaderMode::Deterministic);
            header.set_cksum();

            // Prefix all paths with {name}-{version}/ to match cargo crate format
            let prefixed_path = PathBuf::from(package_prefix).join(relative_path);
            ar.append_data(&mut header, &prefixed_path, &mut file)?;
            uncompressed_size += metadata.len();
        }
    }

    let encoder = ar.into_inner()?;
    encoder.finish()?;

    Ok(uncompressed_size)
}

/// Builds crate metadata from cargo_metadata information.
#[allow(unused)]
fn build_crate_metadata(
    package: &cargo_metadata::Package,
    metadata: &cargo_metadata::Metadata,
) -> anyhow::Result<CrateMetadata> {
    // Collect all workspace dependencies
    let mut all_deps = BTreeMap::new();

    for pkg in &metadata.packages {
        for dep in &pkg.dependencies {
            let dep_key = format!("{}:{}", dep.name, dep.kind);
            all_deps.entry(dep_key).or_insert_with(|| {
                let is_local = dep.path.is_some();
                
                Dep::new(
                    dep.name.clone(),
                    dep.req.to_string(),
                    dep.features.clone(),
                    dep.optional,
                    dep.uses_default_features,
                    dep.target.clone().map(|t| t.to_string()),
                    match dep.kind {
                        cargo_metadata::DependencyKind::Normal => super::schemas::DepKind::Normal,
                        cargo_metadata::DependencyKind::Development => super::schemas::DepKind::Dev,
                        cargo_metadata::DependencyKind::Build => super::schemas::DepKind::Build,
                        cargo_metadata::DependencyKind::Unknown => super::schemas::DepKind::Normal,
                    },
                    if is_local {
                        None
                    } else {
                        match &dep.registry {
                            some @ Some(..) => some.clone(),
                            None => {
                                Some("https://github.com/rust-lang/crates.io-index".to_string())
                            }
                        }
                    },
                    None,
                )
            });
        }
    }

    let deps: Vec<Dep> = all_deps.into_values().collect();

    // Collect all workspace features
    let mut all_features = BTreeMap::new();
    for pkg in &metadata.packages {
        for (feature_name, feature_deps) in &pkg.features {
            all_features
                .entry(feature_name.clone())
                .or_insert_with(Vec::new)
                .extend(feature_deps.clone());
        }
    }

    // Deduplicate feature dependencies
    for feature_deps in all_features.values_mut() {
        feature_deps.sort();
        feature_deps.dedup();
    }

    Ok(CrateMetadata::new(
        package.name.to_string(),
        package.version.to_string(),
        deps,
        all_features,
        package.authors.clone(),
        package.description.clone(),
        package.documentation.clone(),
        package.homepage.clone(),
        None, // readme_file
        package.readme.clone().map(|r| r.to_string()),
        package.keywords.clone(),
        package.categories.clone(),
        package.license.clone(),
        package.license_file.clone().map(|l| l.to_string()),
        package.repository.clone(),
        BTreeMap::new(), // badges
        package.links.clone(),
    ))
}

/// Discovers a git repository starting from the given path.
fn discover_gix_repo(root: &Path) -> anyhow::Result<Option<gix::Repository>> {
    let repo = match gix::ThreadSafeRepository::discover(root) {
        Ok(repo) => repo.to_thread_local(),
        Err(_) => return Ok(None),
    };

    let repo_root = repo.workdir().ok_or_else(|| {
        anyhow::format_err!(
            "Did not expect repo at {} to be bare",
            repo.path().display()
        )
    })?;

    // Verify the repository contains the workspace root
    let canon_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let canon_repo_root = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());

    if canon_root.starts_with(&canon_repo_root) {
        Ok(Some(repo))
    } else {
        Ok(None)
    }
}

/// Lists files using git to respect .gitignore rules.
fn list_files_gix(
    workspace_root: &Path,
    repo: &gix::Repository,
    filter: &impl Fn(&Path, bool) -> bool,
) -> anyhow::Result<Vec<PathBuf>> {
    let options = repo
        .dirwalk_options()?
        .emit_untracked(gix::dir::walk::EmissionMode::Matching)
        .emit_ignored(None)
        .emit_tracked(true)
        .recurse_repositories(false)
        .symlinks_to_directories_are_ignored_like_directories(true);

    let index = repo.index_or_empty()?;
    let mut files = Vec::new();

    for entry in repo.dirwalk_iter(index.clone(), None::<&str>, Default::default(), options)? {
        let entry = entry?;

        let file_path = workspace_root.join(gix::path::from_bstr(entry.entry.rela_path));
        let is_dir = file_path.is_dir();

        if filter(&file_path, is_dir) {
            if !is_dir {
                files.push(file_path);
            } else {
                // Recursively walk directories
                match gix::open(&file_path) {
                    Ok(sub_repo) => {
                        files.extend(list_files_gix(workspace_root, &sub_repo, filter)?);
                    }
                    Err(_) => {
                        list_files_walk(&file_path, &mut files, false, filter)?;
                    }
                }
            }
        }
    }

    Ok(files)
}

/// Lists files by walking the filesystem (fallback when git is not available).
fn list_files_walk(
    path: &Path,
    files: &mut Vec<PathBuf>,
    _is_root: bool,
    filter: &impl Fn(&Path, bool) -> bool,
) -> anyhow::Result<()> {
    if !path.is_dir() {
        return Ok(());
    }

    let walker = walkdir::WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if e.file_type().is_dir() {
                filter(e.path(), true)
            } else {
                true
            }
        });

    for entry in walker {
        match entry {
            Ok(entry) => {
                let file_path = entry.path();

                if file_path.is_file() && filter(file_path, false) {
                    files.push(file_path.to_path_buf());
                }
            }
            Err(err) => match err.path() {
                Some(path) if !filter(path, path.is_dir()) => {}
                Some(path) => files.push(path.to_path_buf()),
                None => return Err(err.into()),
            },
        }
    }

    Ok(())
}

/// Formats a byte count into a human-readable string.
fn human_readable_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit_idx = 0;

    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }

    format!("{:.2} {}", size, UNITS[unit_idx])
}
