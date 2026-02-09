//! Cargo check tool for validating workspace code
//!
//! This module provides functionality for running `cargo check` on the workspace
//! with cancellation support and structured error reporting.

use std::path::PathBuf;
use std::{io::BufReader, path::Path, process::Stdio, sync::Arc};

use crate::event::Reporter;
use crate::execution::cancellable::{CancellableProcess, CancellableResult, CancellationToken};
use crate::tools::cargo;

/// Error types for cargo check operations
#[derive(thiserror::Error, Debug)]
pub enum CheckError {
    #[error("Failed to spawn cargo check process: {0}")]
    SpawnFailed(String),

    #[error("Check was cancelled")]
    Cancelled,

    #[error("Check failed with errors")]
    CheckFailed { diagnostics: String },
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
    ) -> Result<(), CheckError> {
        let cancel_token = CancellationToken::new();
        self.check_cancellable(config, &cancel_token, event_reporter)
    }

    /// Run cargo check with cancellation support
    pub fn check_cancellable(
        &self,
        config: &CheckConfig,
        cancel_token: &CancellationToken,
        event_reporter: Option<Arc<CheckEventReporter>>,
    ) -> Result<(), CheckError> {
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
            .stderr(Stdio::piped())
            .stdin(Stdio::null());

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

        let (diagnostics_tx, diagnostics_rx) = std::sync::mpsc::channel();

        // Capture and parse cargo messages
        if let Some(stdout) = child.stdout.take() {
            let reader = BufReader::new(stdout);
            let reporter_clone = event_reporter.clone();

            std::thread::spawn(move || {
                let stream = cargo_metadata::Message::parse_stream(reader);
                for message in stream.flatten() {
                    match message {
                        cargo_metadata::Message::CompilerMessage(msg) => {
                            if let Some(ref reporter) = reporter_clone {
                                reporter.report_event(CheckEvent {
                                    message: msg.message.message.clone(),
                                });
                            }

                            let Some(rendered) = msg.message.rendered else {
                                continue;
                            };

                            if msg.message.level
                                == cargo_metadata::diagnostic::DiagnosticLevel::Error
                            {
                                let _ = diagnostics_tx.send(rendered.to_string());
                            }
                        }
                        cargo_metadata::Message::CompilerArtifact(artifact) => {
                            if let Some(ref reporter) = reporter_clone {
                                reporter.report_event(CheckEvent {
                                    message: format!("Checking: {}", artifact.target.name),
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
                let diagnostics = diagnostics_rx.try_iter().collect::<Vec<_>>().join("\n");

                if status.success() {
                    if let Some(ref reporter) = event_reporter {
                        reporter.report_event(CheckEvent {
                            message: "Check completed successfully".to_string(),
                        });
                    }
                    Ok(())
                } else {
                    if let Some(ref reporter) = event_reporter {
                        reporter.report_event(CheckEvent {
                            message: "Check failed with errors".to_string(),
                        });
                    }
                    Err(CheckError::CheckFailed { diagnostics })
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
