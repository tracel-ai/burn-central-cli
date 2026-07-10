use tracel_client::Client;
use tracel_client::request::CreateModelRequest;

use crate::context::CliContext;

pub fn ensure_model_exists(
    context: &CliContext,
    client: &Client,
    namespace: &str,
    project: &str,
    model_name: &str,
) -> anyhow::Result<()> {
    match client.get_model(namespace, project, model_name) {
        Ok(_) => return Ok(()),
        Err(e) if e.is_not_found() => {}
        Err(e) => anyhow::bail!("Failed to check model '{model_name}': {e}"),
    }

    let create = context.terminal().confirm(&format!(
        "Model '{model_name}' does not exist in {namespace}/{project}. Create it now?"
    ))?;

    if !create {
        anyhow::bail!("Model upload cancelled: model '{model_name}' does not exist.");
    }

    let description = cliclack::input("Enter model description (optional)")
        .required(false)
        .interact::<String>()
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    client.create_model(
        namespace,
        project,
        CreateModelRequest {
            name: model_name.to_string(),
            description,
        },
    )?;

    context.terminal().print_success(&format!(
        "Created model '{model_name}' in {namespace}/{project}."
    ));

    Ok(())
}
