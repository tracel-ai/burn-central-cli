pub mod cancellable;
pub mod local;

use serde::{Deserialize, Serialize};
use strum::EnumString;

/// Types of procedures that can be executed
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ProcedureType {
    Training,
    Inference,
}

impl std::fmt::Display for ProcedureType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProcedureType::Training => write!(f, "training"),
            ProcedureType::Inference => write!(f, "inference"),
        }
    }
}

/// Build profiles supported
#[derive(Default, Debug, Clone, PartialEq)]
pub enum BuildProfile {
    Debug,
    #[default]
    Release,
}

impl BuildProfile {
    pub fn as_cargo_arg(&self) -> &'static str {
        match self {
            BuildProfile::Debug => "--profile=dev",
            BuildProfile::Release => "--profile=release",
        }
    }
}

#[derive(Debug, Clone, EnumString, Default, Deserialize, Serialize, PartialEq, Eq)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum BackendType {
    #[default]
    Wgpu,
    Tch,
    Ndarray,
    Cuda,
    Rocm,
    Metal,
    Vulkan,
    Cpu,
    Candle,
}

/// Error types specific to execution
#[derive(thiserror::Error, Debug)]
pub enum ExecutionError {
    #[error("Code generation failed: {0}")]
    CodeGenerationFailed(String),

    #[error("Build failed: {message}")]
    BuildFailed {
        message: String,
        diagnostics: Option<String>,
    },

    #[error("Runtime execution failed: {0}")]
    RuntimeFailed(String),

    #[error("Function discovery failed: {0}")]
    FunctionDiscovery(String),

    #[error("Execution cancelled")]
    Cancelled,
}
