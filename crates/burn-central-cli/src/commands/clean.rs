/// Handle the 'clean' command
use crate::helpers::require_linked_project;
use std::fs;
use std::path::Path;

/// Calculate the size of a directory recursively in bytes
fn calculate_dir_size(path: &Path) -> std::io::Result<u64> {
    let mut total_size = 0u64;

    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                total_size += calculate_dir_size(&path)?;
            } else {
                total_size += entry.metadata()?.len();
            }
        }
    }

    Ok(total_size)
}

/// Format bytes into a human-readable string
fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];

    if bytes == 0 {
        return "0 B".to_string();
    }

    let mut size = bytes as f64;
    let mut unit_index = 0;

    while size >= 1024.0 && unit_index < UNITS.len() - 1 {
        size /= 1024.0;
        unit_index += 1;
    }

    if unit_index == 0 {
        format!("{} {}", bytes, UNITS[unit_index])
    } else {
        format!("{:.2} {}", size, UNITS[unit_index])
    }
}

pub fn handle_command(context: crate::context::CliContext) -> Result<(), anyhow::Error> {
    let project = require_linked_project(&context)?;

    context.terminal().command_title("Clean");

    let burn_dir = project.burn_dir();
    let target_dir = burn_dir.target_dir();
    let artifacts_dir = burn_dir.artifacts_dir();
    let crates_dir = burn_dir.crates_dir();
    let bin_dir = burn_dir.bin_dir();
    let dirs = [&target_dir, &artifacts_dir, &crates_dir, &bin_dir];

    let spinner = context.terminal().spinner();
    spinner.start("Calculating space usage...");

    let mut total_size = 0;
    for dir in dirs.iter() {
        if dir.exists() {
            let dir_size = calculate_dir_size(dir).unwrap_or(0);
            total_size += dir_size;
        }
    }

    if total_size == 0 {
        spinner.stop("Nothing to clean.");
        context.terminal().finalize("Clean completed.");
        return Ok(());
    }

    spinner.set_message("Removing local files...");

    let mut errors = Vec::new();

    for dir in dirs.iter() {
        if dir.exists() {
            match fs::remove_dir_all(dir) {
                Ok(_) => {}
                Err(e) => {
                    errors.push(format!(
                        "Failed to remove directory {}: {}",
                        dir.display(),
                        e
                    ));
                }
            }
        }
    }

    let cache = burn_dir.load_cache();
    if let Ok(mut cache) = cache {
        cache.clear();
        _ = burn_dir.save_cache(&cache);
    }

    if !errors.is_empty() {
        spinner.error("Cleaning failed");
        for error in errors {
            context.terminal().print_err(&error);
        }
        context
            .terminal()
            .cancel_finalize("Clean completed with errors");
        return Err(anyhow::anyhow!("Failed to clean some directories"));
    }

    spinner.stop(format!(
        "Cleaning completed, freed {}",
        format_bytes(total_size),
    ));

    context.terminal().finalize("Clean completed successfully.");

    Ok(())
}
