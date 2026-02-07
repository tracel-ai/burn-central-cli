use anyhow::Context;
use burn_central_workspace::ProjectContext;
use burn_central_workspace::compute_provider::TrainingJobArgs;
use burn_central_workspace::execution::BackendType;
use burn_central_workspace::execution::ExecutionError;
use burn_central_workspace::execution::ProcedureType;
use burn_central_workspace::execution::cancellable::CancellationToken;
use burn_central_workspace::execution::local::ExecutionEvent;
use burn_central_workspace::execution::local::ExecutionEventReporter;
use burn_central_workspace::execution::local::LocalExecutionConfig;
use burn_central_workspace::execution::local::LocalExecutor;
use burn_central_workspace::tools::functions_registry::FunctionId;
use burn_central_workspace::tools::functions_registry::FunctionRegistry;
use clap::Parser;
use clap::ValueHint;
use cliclack::{MultiProgress, ProgressBar};
use colored::Colorize;
use ctrlc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::commands::package::package_sequence;
use crate::helpers::{require_linked_project, validate_project_exists_on_server};
use crate::tools::preload_functions;

use crate::context::CliContext;
use crate::tools::terminal::Terminal;

/// Parse a key=value string into a key-value pair
pub fn parse_key_val(s: &str) -> Result<(String, serde_json::Value), String> {
    let (key, value) = s
        .split_once('=')
        .ok_or_else(|| format!("Invalid key=value format: {}", s))?;

    let json_value = serde_json::from_str(value)
        .unwrap_or_else(|_| serde_json::Value::String(value.to_string()));

    Ok((key.to_string(), json_value))
}

#[derive(Parser, Debug)]
pub struct TrainingArgs {
    /// The package name containing the training function
    #[clap(short, long)]
    package: Option<String>,
    /// The training function to run. Annotate a training function with #[burn(training)] to register it.
    function: Option<String>,
    /// Backend to use
    #[clap(short = 'b', long = "backend")]
    backend: Option<BackendType>,
    /// A JSON file containing argument overrides for the training function
    #[clap(long = "args")]
    args: Option<String>,
    /// Batch override: e.g. --overrides a.b=3 x.y.z=true
    #[clap(long = "overrides", value_parser = parse_key_val, value_hint = ValueHint::Other, value_delimiter = ' ', num_args = 1..)]
    overrides: Vec<(String, serde_json::Value)>,
    /// Code version
    #[clap(
        long = "version",
        help = "The code version on which to run the training. (if unspecified, the current version will be packaged and used)"
    )]
    code_version: Option<String>,
    /// The compute provider group name
    #[clap(long = "compute-provider", help = "The compute provider group name.")]
    compute_provider: Option<String>,
}

impl Default for TrainingArgs {
    /// Default config when running the cargo run command
    fn default() -> Self {
        Self {
            package: None,
            function: None,
            args: None,
            overrides: vec![],
            code_version: None,
            compute_provider: None,
            backend: None,
        }
    }
}

pub(crate) fn handle_command(args: TrainingArgs, context: CliContext) -> anyhow::Result<()> {
    let project = require_linked_project(&context)?;

    match args.compute_provider {
        Some(_) => execute_remotely(args, &context, &project),
        None => execute_locally(args, &context, &project),
    }
}

fn prompt_function(function_ids: Vec<FunctionId>) -> anyhow::Result<FunctionId> {
    cliclack::select("Select the function you want to run")
        .items(
            function_ids
                .into_iter()
                .map(|id| {
                    (
                        id.clone(),
                        format!("[{}] {}", id.package_name, id.function_name),
                        "",
                    )
                })
                .collect::<Vec<_>>()
                .as_slice(),
        )
        .filter_mode()
        .interact()
        .map_err(anyhow::Error::from)
}

fn execute_remotely(
    args: TrainingArgs,
    context: &CliContext,
    project_ctx: &ProjectContext,
) -> anyhow::Result<()> {
    context
        .terminal()
        .command_title("Remote training execution");

    let client = context.create_client()?;

    validate_project_exists_on_server(context, project_ctx, &client)?;

    let discovery = preload_functions(context, project_ctx)?;

    let bc_project = project_ctx.get_project();
    let compute_provider = args
        .compute_provider
        .context("Compute provider should be provided")?;
    let function = get_function_to_run(args.package, args.function, &discovery)?;

    let code_version = match args.code_version {
        Some(version) => {
            context
                .terminal()
                .print(&format!("Using code version: {}", version));
            version
        }
        None => {
            context
                .terminal()
                .print("Packaging project to create a new code version...");
            package_sequence(context, project_ctx, Some(&discovery), false)?.digest
        }
    };

    let launch_args = ExperimentConfig::load_config(args.args, args.overrides)?;

    let command = TrainingJobArgs {
        package: Some(function.package_name.clone()),
        function: function.function_name.clone(),
        backend: args.backend.unwrap_or_default(),
        args: Some(launch_args.data),
    };

    let command = serde_json::to_string(&command)?;
    client
        .start_remote_job(
            &compute_provider,
            &bc_project.owner,
            &bc_project.name,
            &code_version,
            &command,
        )
        .with_context(|| {
            format!(
                "Failed to submit training job for function `{}` to compute provider `{}`",
                function, compute_provider
            )
        })?;

    context.terminal().print_success(&format!(
        "Training job for function `{}` has been submitted to compute provider `{}`.",
        function, compute_provider
    ));

    context
        .terminal()
        .finalize("Remote training execution queued successfully.");

    Ok(())
}

struct TrainingReporter {
    multi_progress: MultiProgress,
    main_progress: ProgressBar,
    step_start_time: Mutex<Option<Instant>>,
    current_step: Mutex<Option<String>>,
    current_message: Mutex<String>,
    experiment_num: Arc<Mutex<Option<i32>>>,
}

impl TrainingReporter {
    pub fn new(
        function: &FunctionId,
        terminal: Terminal,
        experiment_num: Arc<Mutex<Option<i32>>>,
    ) -> Self {
        let multi_progress = terminal.multiprogress(&format!(
            "Executing training function `{}`",
            function.to_string().bold()
        ));
        let main_progress = multi_progress.add(terminal.spinner());

        Self {
            multi_progress,
            main_progress,
            step_start_time: Mutex::new(None),
            current_step: Mutex::new(None),
            current_message: Mutex::new("Processing...".to_string()),
            experiment_num,
        }
    }

    fn add_to_history(&self, message: String) {
        self.multi_progress
            .println(format!("  {}", message.dimmed()));
    }

    pub fn push_info(&self, note: String) {
        self.multi_progress.println(format!("  {}", note));
    }

    pub fn update_spinner_display(&self) {
        let current_step = self.current_step.lock().unwrap();
        let current_message = self.current_message.lock().unwrap();
        let step_start_time = self.step_start_time.lock().unwrap();

        if let (Some(step), Some(start_time)) = (current_step.as_ref(), *step_start_time) {
            let elapsed_time = crate::tools::time::format_elapsed_time(start_time.elapsed());

            self.main_progress.set_message(format!(
                "{} {} [{}]",
                step.green().bold(),
                current_message.trim(),
                elapsed_time.dimmed()
            ));
        }
    }

    pub fn start(&self, message: String) {
        self.main_progress.start(message);
    }

    pub fn stop(&self, message: String) {
        self.flush_active_step();
        self.main_progress.stop(message);
        self.multi_progress.stop();
    }

    pub fn cancel(&self) {
        let last_message = self.current_message.lock().unwrap().clone();
        self.main_progress.cancel(format!(
            "{} {}",
            "Cancelled:".yellow().bold(),
            last_message.yellow()
        ));
        self.multi_progress.cancel();
    }

    pub fn error(&self, message: String) {
        let current_step = self.current_step.lock().unwrap();
        let current_message = self.current_message.lock().unwrap();

        if let Some(step) = current_step.as_ref() {
            self.main_progress.set_message(format!(
                "{} {} [{}]",
                step.red().bold(),
                current_message.trim(),
                "x".red()
            ));
        }
        self.multi_progress.error(message);
    }

    fn flush_active_step(&self) {
        let current_step = self.current_step.lock().unwrap();
        let current_message = self.current_message.lock().unwrap();

        if let Some(step) = current_step.as_ref() {
            if !current_message.trim().is_empty() && current_message.trim() != "Processing..." {
                let history_msg = format!("{} - {}", step, current_message.trim());
                self.add_to_history(history_msg);
            }
        }
    }
}

impl ExecutionEventReporter for TrainingReporter {
    fn report_event(&self, event: ExecutionEvent) {
        let message = event.message.unwrap_or_else(|| "Processing...".to_string());

        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&message) {
            if let Some(num) = json.get("experiment_num").and_then(|v| v.as_i64()) {
                *self.experiment_num.lock().unwrap() = Some(num as i32);
            }
        }

        let mut current_step = self.current_step.lock().unwrap();
        let mut step_start_time = self.step_start_time.lock().unwrap();

        let is_new_step = current_step.as_ref() != Some(&event.step);

        if is_new_step {
            drop(current_step);
            drop(step_start_time);
            self.flush_active_step();

            current_step = self.current_step.lock().unwrap();
            step_start_time = self.step_start_time.lock().unwrap();

            *current_step = Some(event.step.clone());
            *step_start_time = Some(Instant::now());
        }

        // Update current message
        *self.current_message.lock().unwrap() = message;

        drop(current_step);
        drop(step_start_time);

        self.update_spinner_display();
    }
}

fn execute_locally(
    args: TrainingArgs,
    context: &CliContext,
    project: &ProjectContext,
) -> anyhow::Result<()> {
    context.terminal().command_title("Local training execution");

    let args_json = ExperimentConfig::load_config(args.args, args.overrides)?;

    let discovery = preload_functions(context, project)?;

    let function = get_function_to_run(args.package, args.function, &discovery)
        .inspect_err(|e| {
            context.terminal().print_err(&e.to_string());
        })
        .with_context(|| "Failed to determine the training function to run.")?;

    let code_version = package_sequence(context, project, Some(&discovery), false)?;

    let executor = LocalExecutor::new(project);
    let backend = args.backend.unwrap_or_default();

    let config = LocalExecutionConfig::new(
        context
            .get_api_key()
            .context("No API key available")?
            .to_string(),
        context.get_api_endpoint().to_string(),
        Some(function.package_name.clone()),
        function.function_name.clone(),
        backend,
        ProcedureType::Training,
        code_version.digest,
    )
    .with_args(args_json.data);

    let experiment_num = Arc::new(Mutex::new(None));
    let reporter = Arc::new(TrainingReporter::new(
        &function,
        context.terminal().clone(),
        experiment_num.clone(),
    ));
    reporter.start(format!(
        "Running training function `{}`...",
        function.to_string().bold()
    ));

    let cancel_token = CancellationToken::new();

    let cancel_token_clone = cancel_token.clone();
    let client_clone = context.create_client().ok();
    let experiment_num_clone = experiment_num.clone();
    let project_clone = project.get_project().clone();

    let signal_count = Arc::new(AtomicUsize::new(0));

    let reporter_clone = Arc::downgrade(&reporter);
    ctrlc::set_handler(move || {
        let count = signal_count.fetch_add(1, Ordering::SeqCst);
        let num = *experiment_num_clone.lock().unwrap();
        if let Some(num) = num
            && count == 0
        {
            if let Some(r) = reporter_clone.upgrade() {
                r.push_info(format!(
                    "{}",
                    "Cancellation requested. Press Ctrl-C again to force quit.".yellow()
                ))
            }
            if let Some(client) = &client_clone {
                let _ = client.cancel_experiment(&project_clone.owner, &project_clone.name, num);
            }
        } else {
            if let Some(r) = reporter_clone.upgrade() {
                r.push_info(format!("{}", "Force quitting...".yellow()))
            }
            cancel_token_clone.cancel();
        }
    })
    .expect("Error setting Ctrl-C handler");

    let timer_cancel = Arc::new(AtomicBool::new(true));
    let timer_handle = thread::spawn({
        let timer_cancel = timer_cancel.clone();
        let reporter = reporter.clone();
        move || {
            while timer_cancel.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(1000));
                if timer_cancel.load(Ordering::Relaxed) {
                    reporter.update_spinner_display();
                }
            }
        }
    });

    let result = executor.execute_cancellable(config, &cancel_token, Some(reporter.clone()));

    timer_cancel.store(false, Ordering::Relaxed);
    let _ = timer_handle.join();

    if let Err(e) = &result {
        // reporter.error("An error occurred while executing training function.".to_string());
        match e {
            ExecutionError::BuildFailed {
                message,
                diagnostics,
            } => {
                reporter.error("Training execution failed".to_string());
                context.terminal().print_err(&format!("Error: {}", message));
                if let Some(diagnostics) = diagnostics {
                    context.terminal().print_err(&format!(
                        "{}\n{}\n{}",
                        "=== COMPILATION DIAGNOSTICS ===\n".yellow(),
                        diagnostics,
                        "===============================".yellow()
                    ));
                }
            }
            ExecutionError::Cancelled => {
                reporter.cancel();
            }
            error => {
                reporter.error("Training execution failed".to_string());
                context.terminal().print_err(&format!("Error: {}", error));
            }
        }
        return Err(anyhow::anyhow!("Training execution failed"));
    }
    let result = result.unwrap();

    if result.success {
        reporter.stop("Training executed successfully".to_string());
        context
            .terminal()
            .finalize("Training completed successfully");
    } else {
        reporter.error("Training execution failed".to_string());

        if let Some(output) = result.output {
            context.terminal().print_err(&format!(
                "{}\n{}\n{}",
                "=== EXECUTION LOG ===\n".yellow(),
                output,
                "=====================".yellow()
            ));
        }

        if let Some(error) = result.error {
            context.terminal().print_err(&format!("Error:\n{}", error));
        }

        return Err(anyhow::anyhow!("Training execution failed"));
    }

    Ok(())
}

fn get_function_to_run(
    package: Option<String>,
    function: Option<String>,
    discovery: &FunctionRegistry,
) -> anyhow::Result<FunctionId> {
    match (package, function) {
        (Some(package_name), Some(function_name)) => {
            let function_id = FunctionId {
                package_name: package_name.clone(),
                function_name: function_name.clone(),
            };

            if discovery.get_function_by_id(&function_id).is_some() {
                return Ok(function_id);
            }

            let error_msg = format!(
                "Function `{}` not found in package `{}`.",
                function_name, package_name,
            );
            Err(anyhow::anyhow!(error_msg))
        }
        (None, Some(function_name)) => {
            let packages_functions = discovery.find_packages_for_function_name(&function_name);

            if packages_functions.is_empty() {
                let error_msg = format!("Function `{}` is not available.", function_name);
                return Err(anyhow::anyhow!(error_msg));
            }

            if packages_functions.len() == 1 {
                let (_, package) = &packages_functions[0];
                return Ok(FunctionId {
                    package_name: package.name.to_string(),
                    function_name,
                });
            }

            let error_msg = format!(
                "Function `{}` exists in multiple packages. Please specify the package using --package.",
                function_name,
            );
            Err(anyhow::anyhow!(error_msg))
        }
        (Some(package_name), None) => {
            let package_functions = discovery.get_package_functions_by_name(&package_name);

            if let Some(functions) = package_functions {
                if functions.is_empty() {
                    let error_msg =
                        format!("Package `{}` has no registered functions", package_name);
                    return Err(anyhow::anyhow!(error_msg));
                }

                let function_ids: Vec<FunctionId> = functions
                    .iter()
                    .map(|f| FunctionId {
                        package_name: package_name.clone(),
                        function_name: f.routine_name.clone(),
                    })
                    .collect();

                let selected = prompt_function(function_ids)?;

                return Ok(selected);
            }

            let error_msg = format!("Package `{}` not found.", package_name);
            Err(anyhow::anyhow!(error_msg))
        }
        (None, None) => {
            if discovery.is_empty() {
                return Err(anyhow::anyhow!(
                    "No training functions found in the project"
                ));
            }
            let function_ids = discovery.get_function_ids();
            prompt_function(function_ids)
        }
    }
}

pub struct ExperimentConfig {
    pub data: serde_json::Value,
}

impl ExperimentConfig {
    fn new(value: serde_json::Value) -> Self {
        Self { data: value }
    }

    fn apply_override(&mut self, key_path: &str, value: serde_json::Value) {
        let mut parts = key_path.split('.').peekable();
        let mut target = &mut self.data;

        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                if let serde_json::Value::Object(map) = target {
                    map.insert(part.to_string(), value.clone());
                }
            } else {
                target = target
                    .as_object_mut()
                    .unwrap()
                    .entry(part)
                    .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            }
        }
    }

    pub fn load_config(
        path: Option<String>,
        overrides: Vec<(String, serde_json::Value)>,
    ) -> anyhow::Result<Self> {
        let base_json = if let Some(path) = &path {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read config file at {}", path))?;
            serde_json::from_str(&text)
                .with_context(|| format!("failed to parse config file at {}", path))?
        } else {
            serde_json::json!({})
        };

        let mut config = ExperimentConfig::new(base_json);

        for (key, val) in &overrides {
            config.apply_override(key, val.clone());
        }

        Ok(config)
    }
}
