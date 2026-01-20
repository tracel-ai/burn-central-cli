use crate::context::CliContext;
use anyhow::Context;
use burn_central_workspace::{
    ProjectContext,
    tools::{function_discovery::DiscoveryEvent, functions_registry::FunctionRegistry},
};
use cliclack::{MultiProgress, ProgressBar};
use colored::Colorize;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Reporter for function discovery progress
struct DiscoveryReporter {
    multi_progress: MultiProgress,
    main_progress: ProgressBar,
    current_package: Mutex<Option<String>>,
    current_message: Mutex<String>,
    package_start_time: Mutex<Option<Instant>>,
}

impl DiscoveryReporter {
    fn new(terminal: &crate::tools::terminal::Terminal) -> Self {
        let multi_progress = terminal.multiprogress("Discovering project functions");
        let main_progress = multi_progress.add(terminal.spinner());

        Self {
            multi_progress,
            main_progress,
            current_package: Mutex::new(None),
            current_message: Mutex::new("Starting...".to_string()),
            package_start_time: Mutex::new(None),
        }
    }

    fn add_to_history(&self, message: String) {
        self.multi_progress
            .println(format!("  {}", message.dimmed()));
    }

    fn update_display(&self) {
        let current_package = self.current_package.lock().unwrap();
        let current_message = self.current_message.lock().unwrap();
        let package_start_time = self.package_start_time.lock().unwrap();

        if let (Some(package), Some(start_time)) = (current_package.as_ref(), *package_start_time) {
            let elapsed_time = crate::tools::time::format_elapsed_time(start_time.elapsed());

            self.main_progress.set_message(format!(
                "{} {} [{}]",
                package.green().bold(),
                current_message.trim(),
                elapsed_time.dimmed()
            ));
        }
    }

    fn start(&self) {
        self.main_progress.start("Initializing...");
    }

    fn stop(&self, count: usize) {
        self.flush_active_package();
        self.main_progress.stop(format!(
            "Discovered {} function{}.",
            count,
            if count == 1 { "" } else { "s" }
        ));
        self.multi_progress.stop();
    }

    fn error(&self, message: String) {
        let current_package = self.current_package.lock().unwrap();
        let current_message = self.current_message.lock().unwrap();

        if let Some(package) = current_package.as_ref() {
            self.main_progress.set_message(format!(
                "{} {} [{}]",
                package.red().bold(),
                current_message.trim(),
                "x".red()
            ));
        }
        self.multi_progress.error(message);
    }

    fn flush_active_package(&self) {
        let current_package = self.current_package.lock().unwrap();
        let current_message = self.current_message.lock().unwrap();

        if let Some(package) = current_package.as_ref() {
            if !current_message.trim().is_empty() && current_message.trim() != "Starting..." {
                let history_msg = format!("{} - {}", package, current_message.trim());
                self.add_to_history(history_msg);
            }
        }
    }

    fn report_event(&self, event: DiscoveryEvent) {
        let message = event.message.unwrap_or_else(|| "Analyzing...".to_string());
        let package_name = event.package.name.clone();

        let mut current_package = self.current_package.lock().unwrap();
        let mut package_start_time = self.package_start_time.lock().unwrap();

        let is_new_package = current_package.as_ref() != Some(&package_name);

        if is_new_package {
            drop(current_package);
            drop(package_start_time);
            self.flush_active_package();

            current_package = self.current_package.lock().unwrap();
            package_start_time = self.package_start_time.lock().unwrap();

            *current_package = Some(package_name.clone());
            *package_start_time = Some(Instant::now());
        }

        // Update current message
        *self.current_message.lock().unwrap() = message;

        drop(current_package);
        drop(package_start_time);

        self.update_display();
    }
}

pub fn preload_functions(
    context: &CliContext,
    project: &ProjectContext,
) -> anyhow::Result<FunctionRegistry> {
    let reporter = Arc::new(DiscoveryReporter::new(context.terminal()));
    reporter.start();

    let reporter_clone = Arc::clone(&reporter);
    let functions =
        project
            .load_functions(Some(Arc::new(move |event: DiscoveryEvent| {
                reporter_clone.report_event(event);
            })))
            .inspect_err(|e| {
                reporter.error("Failed to discover project functions.".to_string());
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

    reporter.stop(functions.num_functions());
    Ok(functions)
}
