use crate::execution::BackendType;

#[derive(serde::Deserialize, serde::Serialize)]
pub struct TrainingJobArgs {
    /// The function to run
    pub function: String,
    /// Backend to use
    pub backend: BackendType,
    /// Config file path
    pub args: Option<serde_json::Value>,
}
