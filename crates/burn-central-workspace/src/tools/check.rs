//! Cargo check tool for validating workspace code
//!
//! This module provides functionality for running `cargo check` on the workspace
//! with cancellation support and structured error reporting.
//!
//! # Examples
//!
//! ## Basic usage
//!
//! ```no_run
//! use burn_central_workspace::tools::check::{CargoChecker, CheckConfig};
//! use std::path::Path;
//!
//! let workspace_root = Path::new(".");
//! let checker = CargoChecker::new(workspace_root);
//! let config = CheckConfig::default();
//!
//! match checker.check(&config, None) {
//!     Ok(result) => {
//!         println!("Check passed!");
//!         println!("Warnings: {}", result.warnings.len());
//!     }
//!     Err(e) => eprintln!("Check failed: {}", e),
//! }
//! ```
//!
//! ## With event reporting
//!
//! ```no_run
//! use burn_central_workspace::tools::check::{CargoChecker, CheckConfig, CheckEvent};
//! use std::path::Path;
//! use std::sync::Arc;
//!
//! let workspace_root = Path::new(".");
//! let checker = CargoChecker::new(workspace_root);
//! let config = CheckConfig::default();
//!
//! // Create an event reporter
//! let reporter = Arc::new(|event: CheckEvent| {
//!     println!("[{}] {:?}", event.step, event.message);
//! });
//!
//! match checker.check(&config, Some(reporter)) {
//!     Ok(result) => println!("Success with {} warnings", result.warnings.len()),
//!     Err(e) => eprintln!("Error: {}", e),
//! }
//! ```
//!
//! ## With cancellation support
//!
//! ```no_run
//! use burn_central_workspace::tools::check::{CargoChecker, CheckConfig};
//! use burn_central_workspace::execution::cancellable::CancellationToken;
//! use std::path::Path;
//! use std::sync::Arc;
//! use std::thread;
//!
//! let workspace_root = Path::new(".");
//! let checker = CargoChecker::new(workspace_root);
//! let config = CheckConfig::default();
//! let cancel_token = CancellationToken::new();
//!
//! // Spawn a thread to cancel after 5 seconds
//! let token_clone = cancel_token.clone();
//! thread::spawn(move || {
//!     thread::sleep(std::time::Duration::from_secs(5));
//!     token_clone.cancel();
//! });
//!
//! match checker.check_cancellable(&config, &cancel_token, None) {
//!     Ok(result) => println!("Check completed"),
//!     Err(e) => eprintln!("Check failed or cancelled: {}", e),
//! }
//! ```
//!
//! ## Advanced configuration
//!
//! ```no_run
//! use burn_central_workspace::tools::check::{CargoChecker, CheckConfig};
//! use std::path::Path;
//!
//! let workspace_root = Path::new(".");
//! let checker = CargoChecker::new(workspace_root);
//!
//! let config = CheckConfig {
//!     features: Some(vec!["feature1".to_string(), "feature2".to_string()]),
//!     all_features: false,
//!     no_default_features: false,
//!     package: Some("my-package".to_string()),
//!     tests: true,
//!     benches: false,
//!     examples: true,
//! };
//!
//! match checker.check(&config, None) {
//!     Ok(result) => {
//!         println!("Check passed!");
//!         for warning in &result.warnings {
//!             println!("Warning: {}", warning.message);
//!         }
//!     }
//!     Err(e) => eprintln!("Check failed: {}", e),
//! }
//! ```
//!
//! ## Handling errors and diagnostics
//!
//! ```no_run
//! use burn_central_workspace::tools::check::{CargoChecker, CheckConfig, CheckError};
//! use std::path::Path;
//!
//! let workspace_root = Path::new(".");
//! let checker = CargoChecker::new(workspace_root);
//! let config = CheckConfig::default();
//!
//! match checker.check(&config, None) {
//!     Ok(result) => {
//!         println!("Check passed!");
//!     }
//!     Err(CheckError::CheckFailed(result)) => {
//!         eprintln!("Check failed with {} errors:", result.errors.len());
//!         for error in &result.errors {
//!             eprintln!("  - {}", error.message);
//!             if let Some(ref path) = error.file_path {
//!                 eprintln!("    at {}:{}:{}",
//!                     path,
//!                     error.line.unwrap_or(0),
//!                     error.column.unwrap_or(0)
//!                 );
//!             }
//!             if let Some(ref rendered) = error.rendered {
//!                 eprintln!("{}", rendered);
//!             }
//!         }
//!     }
//!     Err(CheckError::Cancelled) => {
//!         eprintln!("Check was cancelled");
//!     }
//!     Err(e) => {
//!         eprintln!("Unexpected error: {}", e);
//!     }
//! }
//! ```

use std::path::PathBuf;
use std::{io::BufReader, path::Path, process::Stdio, sync::Arc};

use crate::event::Reporter;
use crate::execution::cancellable::{CancellableProcess, CancellableResult, CancellationToken};
use crate::tools::cargo;

/// Result of a cargo check operation
#[derive(Debug, Clone)]
pub struct CheckResult {
    /// Whether the check succeeded without errors
    pub success: bool,
    /// Warnings encountered during check
    pub warnings: Vec<CheckDiagnostic>,
    /// Errors encountered during check
    pub errors: Vec<CheckDiagnostic>,
    /// Raw output from cargo check
    pub output: String,
}

/// A diagnostic message from cargo check
#[derive(Debug, Clone)]
pub struct CheckDiagnostic {
    /// The diagnostic level (error, warning, etc.)
    pub level: DiagnosticLevel,
    /// The main message
    pub message: String,
    /// Rendered output (colorized, formatted)
    pub rendered: Option<String>,
    /// File path where the diagnostic occurred
    pub file_path: Option<String>,
    /// Line number where the diagnostic occurred
    pub line: Option<u32>,
    /// Column number where the diagnostic occurred
    pub column: Option<u32>,
}

/// Diagnostic severity level
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticLevel {
    Error,
    Warning,
    Note,
    Help,
    Other(String),
}

impl From<cargo_metadata::diagnostic::DiagnosticLevel> for DiagnosticLevel {
    fn from(level: cargo_metadata::diagnostic::DiagnosticLevel) -> Self {
        match level {
            cargo_metadata::diagnostic::DiagnosticLevel::Error => DiagnosticLevel::Error,
            cargo_metadata::diagnostic::DiagnosticLevel::Warning => DiagnosticLevel::Warning,
            cargo_metadata::diagnostic::DiagnosticLevel::Note => DiagnosticLevel::Note,
            cargo_metadata::diagnostic::DiagnosticLevel::Help => DiagnosticLevel::Help,
            _ => DiagnosticLevel::Other(format!("{:?}", level)),
        }
    }
}

impl CheckResult {
    /// Create a successful check result
    pub fn success() -> Self {
        Self {
            success: true,
            warnings: Vec::new(),
            errors: Vec::new(),
            output: String::new(),
        }
    }

    /// Create a failed check result
    pub fn failed(errors: Vec<CheckDiagnostic>, warnings: Vec<CheckDiagnostic>) -> Self {
        Self {
            success: false,
            warnings,
            errors,
            output: String::new(),
        }
    }

    /// Create a cancelled check result
    pub fn cancelled() -> Self {
        Self {
            success: false,
            warnings: Vec::new(),
            errors: Vec::new(),
            output: "Check was cancelled".to_string(),
        }
    }
}

/// Error types for cargo check operations
#[derive(thiserror::Error, Debug)]
pub enum CheckError {
    #[error("Failed to spawn cargo check process: {0}")]
    SpawnFailed(String),

    #[error("Check was cancelled")]
    Cancelled,

    #[error("Check failed with errors")]
    CheckFailed(CheckResult),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
}

/// Event emitted during check operation
#[derive(Debug, Clone)]
pub struct CheckEvent {
    pub message: String,
}

type CheckEventReporter = dyn Reporter<CheckEvent>;

/// Configuration for cargo check
#[derive(Debug, Clone, Default)]
pub struct CheckConfig {
    /// Optional target directory for cargo check
    pub target_dir: Option<PathBuf>,
    /// Whether or not to include RUSTC_BOOTSTRAP environment variable
    pub bootstrap: bool,
}

/// Cargo check executor
pub struct CargoChecker<'a> {
    workspace_root: &'a Path,
}

impl<'a> CargoChecker<'a> {
    /// Create a new cargo checker for the given workspace
    pub fn new(workspace_root: &'a Path) -> Self {
        Self { workspace_root }
    }

    /// Run cargo check without cancellation support
    pub fn check(
        &self,
        config: &CheckConfig,
        event_reporter: Option<Arc<CheckEventReporter>>,
    ) -> Result<CheckResult, CheckError> {
        let cancel_token = CancellationToken::new();
        self.check_cancellable(config, &cancel_token, event_reporter)
    }

    /// Run cargo check with cancellation support
    pub fn check_cancellable(
        &self,
        config: &CheckConfig,
        cancel_token: &CancellationToken,
        event_reporter: Option<Arc<CheckEventReporter>>,
    ) -> Result<CheckResult, CheckError> {
        if let Some(ref reporter) = event_reporter {
            reporter.report_event(CheckEvent {
                message: "Starting cargo check...".to_string(),
            });
        }

        if cancel_token.is_cancelled() {
            return Err(CheckError::Cancelled);
        }

        let mut check_cmd = cargo::command();
        check_cmd
            .current_dir(self.workspace_root)
            .arg("check")
            .arg("--message-format=json")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if config.bootstrap {
            check_cmd.env("RUSTC_BOOTSTRAP", "1");
        }

        if let Some(ref target_dir) = config.target_dir {
            check_cmd.arg("--target-dir").arg(target_dir);
        }

        if let Some(ref reporter) = event_reporter {
            reporter.report_event(CheckEvent {
                message: "Executing cargo check...".to_string(),
            });
        }

        let mut child = check_cmd.spawn().map_err(|e| {
            let error_msg = format!("Failed to spawn cargo check: {}", e);
            if let Some(ref reporter) = event_reporter {
                reporter.report_event(CheckEvent {
                    message: error_msg.clone(),
                });
            }
            CheckError::SpawnFailed(error_msg)
        })?;

        let (errors_tx, errors_rx) = std::sync::mpsc::channel();
        let (warnings_tx, warnings_rx) = std::sync::mpsc::channel();

        // Capture and parse cargo messages
        if let Some(stdout) = child.stdout.take() {
            let reader = BufReader::new(stdout);
            let reporter_clone = event_reporter.clone();

            std::thread::spawn(move || {
                let stream = cargo_metadata::Message::parse_stream(reader);
                for message in stream.flatten() {
                    match message {
                        cargo_metadata::Message::CompilerMessage(msg) => {
                            let diagnostic = CheckDiagnostic {
                                level: msg.message.level.into(),
                                message: msg.message.message.clone(),
                                rendered: msg.message.rendered.clone(),
                                file_path: msg
                                    .message
                                    .spans
                                    .first()
                                    .map(|span| span.file_name.clone()),
                                line: msg.message.spans.first().map(|span| span.line_start as u32),
                                column: msg
                                    .message
                                    .spans
                                    .first()
                                    .map(|span| span.column_start as u32),
                            };

                            if let Some(ref reporter) = reporter_clone {
                                reporter.report_event(CheckEvent {
                                    message: msg.message.message.clone(),
                                });
                            }

                            match msg.message.level {
                                cargo_metadata::diagnostic::DiagnosticLevel::Error => {
                                    let _ = errors_tx.send(diagnostic);
                                }
                                cargo_metadata::diagnostic::DiagnosticLevel::Warning => {
                                    let _ = warnings_tx.send(diagnostic);
                                }
                                _ => {}
                            }
                        }
                        cargo_metadata::Message::CompilerArtifact(artifact) => {
                            if let Some(ref reporter) = reporter_clone {
                                reporter.report_event(CheckEvent {
                                    message: format!("Checking: {}", artifact.package_id.repr),
                                });
                            }
                        }
                        _ => {}
                    }
                }
            });
        }

        let cancellable = CancellableProcess::new(child, cancel_token.clone());
        let result = cancellable.wait();

        match result {
            CancellableResult::Completed(status) => {
                let errors: Vec<CheckDiagnostic> = errors_rx.try_iter().collect();
                let warnings: Vec<CheckDiagnostic> = warnings_rx.try_iter().collect();

                if status.success() {
                    if let Some(ref reporter) = event_reporter {
                        reporter.report_event(CheckEvent {
                            message: format!(
                                "Check completed successfully ({} warnings)",
                                warnings.len()
                            ),
                        });
                    }
                    Ok(CheckResult {
                        success: true,
                        warnings,
                        errors,
                        output: String::new(),
                    })
                } else {
                    if let Some(ref reporter) = event_reporter {
                        reporter.report_event(CheckEvent {
                            message: format!(
                                "Check failed with {} errors and {} warnings",
                                errors.len(),
                                warnings.len()
                            ),
                        });
                    }
                    let result = CheckResult::failed(errors, warnings);
                    Err(CheckError::CheckFailed(result))
                }
            }
            CancellableResult::Cancelled => {
                if let Some(ref reporter) = event_reporter {
                    reporter.report_event(CheckEvent {
                        message: "Check cancelled by user".to_string(),
                    });
                }
                Err(CheckError::Cancelled)
            }
        }
    }
}
