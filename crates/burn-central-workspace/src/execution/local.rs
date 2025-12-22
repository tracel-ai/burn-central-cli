//! Local execution core for Burn Central
//!
//! This module provides the core functionality for building and executing functions locally.

use serde::Serialize;

use crate::{
    entity::projects::ProjectContext,
    execution::{
        BackendType, BuildProfile, ExecutionError, ProcedureType, cancellable::CancellationToken,
    },
    tools::{cargo, function_discovery::FunctionMetadata},
};
use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
};

use crate::execution::cancellable::{CancellableProcess, CancellableResult};

/// Configuration for executing a function locally
#[derive(Debug, Clone)]
pub struct LocalExecutionConfig {
    /// The API key of the user in Burn Central
    pub api_key: String,
    /// The API endpoint to use
    pub api_endpoint: String,
    /// The function to execute
    pub function: String,
    /// Backend to use for execution
    pub backend: BackendType,
    /// Launch arguments
    pub args: serde_json::Value,
    /// Type of procedure to execute
    pub procedure_type: ProcedureType,
    /// Build profile (debug/release)
    pub build_profile: BuildProfile,
    /// Code version/digest for tracking
    pub code_version: String,
}

struct BuildConfig {
    pub backend: BackendType,
    pub build_profile: BuildProfile,
    pub code_version: String,
}

struct RunConfig {
    pub function: String,
    pub procedure_type: ProcedureType,
    pub args: serde_json::Value,
    pub api_key: String,
    pub api_endpoint: String,
}

impl LocalExecutionConfig {
    /// Create a new local execution config
    pub fn new(
        api_key: String,
        api_endpoint: String,
        function: String,
        backend: BackendType,
        procedure_type: ProcedureType,
        code_version: String,
    ) -> Self {
        Self {
            api_key,
            api_endpoint,
            function,
            backend,
            procedure_type,
            code_version,
            args: serde_json::Value::Null,
            build_profile: BuildProfile::default(),
        }
    }

    pub fn with_args<A: Serialize>(mut self, args: A) -> Self {
        self.args = serde_json::to_value(args).unwrap_or(serde_json::Value::Null);
        self
    }

    /// Set the build profile
    pub fn with_build_profile(mut self, profile: BuildProfile) -> Self {
        self.build_profile = profile;
        self
    }
}

/// Result of a local execution
#[derive(Debug)]
pub struct LocalExecutionResult {
    /// Whether the execution was successful
    pub success: bool,
    /// Output from the execution
    pub output: Option<String>,
    /// Error message if execution failed
    pub error: Option<String>,
    /// Exit code if available
    pub exit_code: Option<i32>,
}

impl LocalExecutionResult {
    /// Create a successful result
    pub fn success(output: Option<String>) -> Self {
        Self {
            success: true,
            output,
            error: None,
            exit_code: Some(0),
        }
    }

    /// Create a failed result
    pub fn failure(error: String, exit_code: Option<i32>, output: Option<String>) -> Self {
        Self {
            success: false,
            output,
            error: Some(error),
            exit_code,
        }
    }

    /// Create a cancelled result
    pub fn cancelled() -> Self {
        Self {
            success: false,
            output: None,
            error: Some("Execution cancelled by user".to_string()),
            exit_code: Some(-1),
        }
    }
}

pub struct ExecutionEvent {
    pub step: String,
    pub message: Option<String>,
}

pub trait ExecutionEventReporter: Send + Sync {
    fn report_event(&self, event: ExecutionEvent);
}

impl ExecutionEventReporter for () {
    fn report_event(&self, _event: ExecutionEvent) {
        // No-op
    }
}

impl<F> ExecutionEventReporter for F
where
    F: Fn(ExecutionEvent) + Send + Sync,
{
    fn report_event(&self, event: ExecutionEvent) {
        (self)(event);
    }
}

impl ExecutionEventReporter for std::sync::mpsc::Sender<ExecutionEvent> {
    fn report_event(&self, event: ExecutionEvent) {
        let _ = self.send(event);
    }
}

/// Core local executor - handles building and running functions locally
pub struct LocalExecutor<'a> {
    project: &'a ProjectContext,
}

impl<'a> LocalExecutor<'a> {
    /// Create a new local executor
    pub fn new(project: &'a ProjectContext) -> Self {
        Self { project }
    }

    /// Execute a function locally
    pub fn execute(
        &self,
        config: LocalExecutionConfig,
        event_reporter: Option<Arc<dyn ExecutionEventReporter>>,
    ) -> Result<LocalExecutionResult, ExecutionError> {
        let cancellation_token = CancellationToken::new();
        self.execute_cancellable(config, &cancellation_token, event_reporter)
    }

    /// Execute a function locally with cancellation support
    pub fn execute_cancellable(
        &self,
        config: LocalExecutionConfig,
        cancel_token: &CancellationToken,
        event_reporter: Option<Arc<dyn ExecutionEventReporter>>,
    ) -> Result<LocalExecutionResult, ExecutionError> {
        let functions = self
            .project
            .load_functions_cancellable(cancel_token)
            .map_err(|e| ExecutionError::FunctionDiscovery(format!("{e}")))?;
        let function_refs = functions.get_function_references();
        self.validate_function(&config.function, function_refs)?;

        let build_config = BuildConfig {
            backend: config.backend,
            build_profile: config.build_profile,
            code_version: config.code_version,
        };

        let crate_name = "burn_central_executable";
        let crate_dir = self.generate_executable_crate(
            crate_name,
            &build_config,
            cancel_token,
            event_reporter.clone(),
        )?;

        if cancel_token.is_cancelled() {
            return Ok(LocalExecutionResult::cancelled());
        }

        let executable_path = self.build_executable(
            crate_name,
            &crate_dir,
            &build_config,
            cancel_token,
            event_reporter.clone(),
        )?;

        let run_config = RunConfig {
            function: config.function,
            procedure_type: config.procedure_type,
            args: config.args,
            api_key: config.api_key,
            api_endpoint: config.api_endpoint,
        };
        if cancel_token.is_cancelled() {
            return Ok(LocalExecutionResult::cancelled());
        }

        self.run_executable(&executable_path, &run_config, cancel_token, event_reporter)
    }

    /// Validate that the requested function exists and matches the procedure type
    fn validate_function(
        &self,
        function: &str,
        available_functions: &[FunctionMetadata],
    ) -> Result<(), ExecutionError> {
        let function_names: Vec<&str> = available_functions
            .iter()
            .map(|f| f.routine_name.as_str())
            .collect();

        if !function_names.contains(&function) {
            return Err(ExecutionError::FunctionDiscovery(format!(
                "Function '{}' not found. Available functions: {:?}",
                function, function_names
            ))
            .into());
        }

        Ok(())
    }

    fn generate_executable_crate(
        &self,
        crate_name: &str,
        config: &BuildConfig,
        cancel_token: &CancellationToken,
        event_reporter: Option<Arc<dyn ExecutionEventReporter>>,
    ) -> Result<PathBuf, ExecutionError> {
        if cancel_token.is_cancelled() {
            return Err(ExecutionError::Cancelled);
        }

        if let Some(ref reporter) = event_reporter {
            reporter.report_event(ExecutionEvent {
                step: "codegen".to_string(),
                message: Some(format!("Generating executable crate '{}'", crate_name)),
            });
        }

        let functions = self
            .project
            .load_functions_cancellable(cancel_token)
            .map_err(|e| ExecutionError::FunctionDiscovery(format!("{e}")))?;

        let generated_crate = crate::generation::crate_gen::create_crate(
            crate_name,
            self.project.get_crate_name(),
            self.project.get_crate_path().to_str().unwrap(),
            &config.backend,
            functions.get_function_references(),
            self.project.get_current_package(),
        );

        let mut cache = self.project.burn_dir().load_cache().map_err(|e| {
            ExecutionError::CodeGenerationFailed(format!("Failed to load cache: {}", e))
        })?;

        if cancel_token.is_cancelled() {
            return Err(ExecutionError::Cancelled);
        }
        let crate_path = self.project.burn_dir().crates_dir().join(crate_name);
        generated_crate
            .write_to_burn_dir(&crate_path, &mut cache)
            .map_err(|e| {
                ExecutionError::CodeGenerationFailed(format!(
                    "Failed to write generated crate: {}",
                    e
                ))
            })?;

        if let Some(ref reporter) = event_reporter {
            reporter.report_event(ExecutionEvent {
                step: "codegen".to_string(),
                message: Some(format!(
                    "Generated executable crate at: {}",
                    crate_path.display()
                )),
            });
        }

        Ok(crate_path)
    }

    fn build_executable(
        &self,
        crate_name: &str,
        crate_dir: &Path,
        config: &BuildConfig,
        cancel_token: &CancellationToken,
        event_reporter: Option<Arc<dyn ExecutionEventReporter>>,
    ) -> Result<PathBuf, ExecutionError> {
        let build_dir = crate_dir;

        if let Some(ref reporter) = event_reporter {
            reporter.report_event(ExecutionEvent {
                step: "build".to_string(),
                message: Some(format!("Starting cargo build for crate '{}'", crate_name)),
            });
        }

        let mut build_cmd = cargo::command();
        build_cmd
            .current_dir(build_dir)
            .arg("build")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        build_cmd.arg("--message-format=json");
        build_cmd.arg(config.build_profile.as_cargo_arg());
        build_cmd.env("BURN_CENTRAL_CODE_VERSION", &config.code_version);
        build_cmd.args([
            "--manifest-path",
            &build_dir.join("Cargo.toml").to_string_lossy(),
        ]);

        if let Some(ref reporter) = event_reporter {
            reporter.report_event(ExecutionEvent {
                step: "build".to_string(),
                message: Some("Executing cargo build...".to_string()),
            });
        }

        let mut child = build_cmd.spawn().map_err(|e| {
            let error_msg = format!("Failed to execute cargo build: {}", e);
            if let Some(ref reporter) = event_reporter {
                reporter.report_event(ExecutionEvent {
                    step: "build".to_string(),
                    message: Some(error_msg.clone()),
                });
            }
            ExecutionError::BuildFailed {
                message: error_msg,
                diagnostics: None,
            }
        })?;

        let (binary_path_tx, binary_path_rx) = std::sync::mpsc::channel();
        let (build_errors_tx, build_errors_rx) = std::sync::mpsc::channel();

        // Capture and report stdout (cargo messages)
        if let Some(stdout) = child.stdout.take() {
            let reader = BufReader::new(stdout);
            let reporter_clone = event_reporter.clone();

            std::thread::spawn(move || {
                let stream = cargo_metadata::Message::parse_stream(reader);
                for maybe_message in stream {
                    if let Ok(message) = maybe_message {
                        match message {
                            cargo_metadata::Message::CompilerArtifact(artifact) => {
                                if let Some(executable) = artifact.executable {
                                    let _ = binary_path_tx.send(executable);
                                }
                            }
                            cargo_metadata::Message::CompilerMessage(msg) => {
                                if let Some(ref reporter) = reporter_clone {
                                    reporter.report_event(ExecutionEvent {
                                        step: "build".to_string(),
                                        message: Some(msg.message.message.clone()),
                                    });
                                }
                                let rendered = msg.message.rendered.unwrap_or_default();
                                if matches!(
                                    msg.message.level,
                                    cargo_metadata::diagnostic::DiagnosticLevel::Error
                                ) {
                                    let _ = build_errors_tx.send(rendered);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            });
        }

        let cancellable = CancellableProcess::new(child, cancel_token.clone());
        let result = cancellable.wait();

        match result {
            CancellableResult::Completed(status) => {
                if status.success() {
                    if let Some(ref reporter) = event_reporter {
                        reporter.report_event(ExecutionEvent {
                            step: "build".to_string(),
                            message: Some("Build completed successfully".to_string()),
                        });
                    }
                } else {
                    if let Some(ref reporter) = event_reporter {
                        reporter.report_event(ExecutionEvent {
                            step: "build".to_string(),
                            message: Some("Build failed".to_string()),
                        });
                    }
                    let diagnostics = build_errors_rx
                        .try_iter()
                        .collect::<Vec<String>>()
                        .join("\n");
                    return Err(ExecutionError::BuildFailed {
                        message: "Compiler encountered errors".to_string(),
                        diagnostics: Some(diagnostics),
                    });
                }
            }
            CancellableResult::Cancelled => {
                let error_msg = "Build cancelled by user";
                if let Some(ref reporter) = event_reporter {
                    reporter.report_event(ExecutionEvent {
                        step: "build".to_string(),
                        message: Some(error_msg.to_string()),
                    });
                }
                return Err(ExecutionError::Cancelled);
            }
        }

        let executable_path = binary_path_rx
            .recv()
            .map_err(|_| {
                let error_msg = "Failed to retrieve built executable path".to_string();
                if let Some(ref reporter) = event_reporter {
                    reporter.report_event(ExecutionEvent {
                        step: "build".to_string(),
                        message: Some(error_msg.clone()),
                    });
                }
                ExecutionError::BuildFailed {
                    message: error_msg,
                    diagnostics: None,
                }
            })?
            .into_std_path_buf();

        if !executable_path.exists() {
            let error_msg = format!(
                "Built executable not found at: {}",
                executable_path.display()
            );
            if let Some(ref reporter) = event_reporter {
                reporter.report_event(ExecutionEvent {
                    step: "build".to_string(),
                    message: Some(error_msg.clone()),
                });
            }
            return Err(ExecutionError::BuildFailed {
                message: error_msg,
                diagnostics: None,
            });
        }

        if let Some(ref reporter) = event_reporter {
            reporter.report_event(ExecutionEvent {
                step: "build".to_string(),
                message: Some(format!(
                    "Executable built successfully: {}",
                    executable_path.display()
                )),
            });
        }

        Ok(executable_path)
    }

    /// Execute the built binary with cancellation support
    fn run_executable(
        &self,
        executable_path: &Path,
        config: &RunConfig,
        cancel_token: &CancellationToken,
        event_reporter: Option<Arc<dyn ExecutionEventReporter>>,
    ) -> Result<LocalExecutionResult, ExecutionError> {
        if let Some(ref reporter) = event_reporter {
            reporter.report_event(ExecutionEvent {
                step: "execution".to_string(),
                message: Some(format!(
                    "Starting execution of function '{}'",
                    config.function
                )),
            });
        }

        let mut run_cmd = Command::new(executable_path);

        run_cmd.env("BURN_PROJECT_DIR", self.project.get_crate_path());

        let project = self.project.get_project();
        run_cmd.args(["--namespace", &project.owner]);
        run_cmd.args(["--project", &project.name]);
        run_cmd.args(["--api-key", &config.api_key]);
        run_cmd.args(["--endpoint", &config.api_endpoint]);

        let args_str = serde_json::to_string(&config.args).map_err(|e| {
            let error_msg = format!("Failed to serialize args: {}", e);
            if let Some(ref reporter) = event_reporter {
                reporter.report_event(ExecutionEvent {
                    step: "execution".to_string(),
                    message: Some(error_msg.clone()),
                });
            }
            ExecutionError::RuntimeFailed(error_msg)
        })?;
        run_cmd.args(["--args", &args_str]);

        let run_kind = match config.procedure_type {
            ProcedureType::Training => "train",
            ProcedureType::Inference => "infer",
        };

        run_cmd.arg(run_kind);
        run_cmd.arg(&config.function);

        run_cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::piped());

        if let Some(ref reporter) = event_reporter {
            reporter.report_event(ExecutionEvent {
                step: "execution".to_string(),
                message: Some("Executing binary...".to_string()),
            });
        }

        let mut child = run_cmd.spawn().map_err(|e| {
            let error_msg = format!("Failed to execute binary: {}", e);
            if let Some(ref reporter) = event_reporter {
                reporter.report_event(ExecutionEvent {
                    step: "execution".to_string(),
                    message: Some(error_msg.clone()),
                });
            }
            ExecutionError::RuntimeFailed(error_msg)
        })?;

        let (stdio_tx, stdio_rx) = std::sync::mpsc::channel();

        // Capture and report stdout in real-time
        if let Some(stdout) = child.stdout.take() {
            let reader = BufReader::new(stdout);
            let reporter_clone = event_reporter.clone();
            let stdio_tx_clone = stdio_tx.clone();
            std::thread::spawn(move || {
                for line in reader.lines().map_while(Result::ok) {
                    let _ = stdio_tx_clone.send(line.clone());
                    if let Some(ref reporter) = reporter_clone {
                        reporter.report_event(ExecutionEvent {
                            step: "execution".to_string(),
                            message: Some(line),
                        });
                    }
                }
            });
        }

        // Capture and report stderr in real-time
        if let Some(stderr) = child.stderr.take() {
            let reader = BufReader::new(stderr);
            let reporter_clone = event_reporter.clone();

            std::thread::spawn(move || {
                for line in reader.lines().map_while(Result::ok) {
                    let _ = stdio_tx.send(line.clone());
                    if let Some(ref reporter) = reporter_clone {
                        reporter.report_event(ExecutionEvent {
                            step: "execution".to_string(),
                            message: Some(line),
                        });
                    }
                }
            });
        }

        let cancellable = CancellableProcess::new(child, cancel_token.clone());
        let result = cancellable.wait();

        let output = stdio_rx.iter().collect::<Vec<String>>().join("\n");

        let status = match result {
            CancellableResult::Completed(output) => output,
            CancellableResult::Cancelled => {
                if let Some(ref reporter) = event_reporter {
                    reporter.report_event(ExecutionEvent {
                        step: "execution".to_string(),
                        message: Some("Execution cancelled by user".to_string()),
                    });
                }
                return Ok(LocalExecutionResult::cancelled());
            }
        };

        if status.success() {
            if let Some(ref reporter) = event_reporter {
                reporter.report_event(ExecutionEvent {
                    step: "execution".to_string(),
                    message: Some("Execution completed successfully".to_string()),
                });
            }
            Ok(LocalExecutionResult::success(Some(output)))
        } else {
            let error_message = format!("Execution failed with exit code: {:?}", status.code());

            if let Some(reporter) = event_reporter {
                reporter.report_event(ExecutionEvent {
                    step: "execution".to_string(),
                    message: Some(error_message.clone()),
                });
            }

            Ok(LocalExecutionResult::failure(
                error_message,
                status.code(),
                Some(output),
            ))
        }
    }

    /// List available functions of a specific type
    pub fn list_functions(&self, procedure_type: ProcedureType) -> crate::Result<Vec<String>> {
        let functions = self.project.load_functions()?;
        let filtered_functions: Vec<String> = functions
            .get_function_references()
            .iter()
            .filter(|f| f.proc_type.to_lowercase() == procedure_type.to_string().to_lowercase())
            .map(|f| f.routine_name.clone())
            .collect();

        Ok(filtered_functions)
    }

    /// List available functions of a specific type with cancellation support
    pub fn list_functions_cancellable(
        &self,
        procedure_type: ProcedureType,
        cancellation_token: &CancellationToken,
    ) -> crate::Result<Vec<String>> {
        let functions = self
            .project
            .load_functions_cancellable(cancellation_token)?;
        let filtered_functions: Vec<String> = functions
            .get_function_references()
            .iter()
            .filter(|f| f.proc_type.to_lowercase() == procedure_type.to_string().to_lowercase())
            .map(|f| f.routine_name.clone())
            .collect();

        Ok(filtered_functions)
    }

    /// List all available training functions
    pub fn list_training_functions(&self) -> crate::Result<Vec<String>> {
        self.list_functions(ProcedureType::Training)
    }

    /// List all available inference functions
    pub fn list_inference_functions(&self) -> crate::Result<Vec<String>> {
        self.list_functions(ProcedureType::Inference)
    }
}
