use crate::execution::{BackendType, ProcedureType};

#[derive(serde::Deserialize, serde::Serialize)]
pub struct ComputeProviderJobArgs {
    /// The function to run
    pub function: String,
    /// Backend to use
    pub backend: BackendType,
    /// Config file path
    pub args: Option<serde_json::Value>,
    /// Project version/digest
    pub digest: String,
    /// Project namespace
    pub namespace: String,
    /// Project name
    pub project: String,
    /// API key
    pub key: String,
    /// API endpoint
    pub api_endpoint: String,
    /// Procedure type (training/inference)
    pub procedure_type: ProcedureType,
}
