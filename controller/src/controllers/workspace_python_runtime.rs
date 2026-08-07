use kubimo::{ResourceNameExt as _, ResourceNamespaceExt as _, Workspace, WorkspacePythonRuntime};

use crate::Context;

pub(crate) fn get_workspace_python_runtime(
    workspace: &Workspace,
) -> kubimo::Result<WorkspacePythonRuntime> {
    workspace
        .status
        .as_ref()
        .and_then(|status| status.python_runtime)
        .ok_or_else(|| {
            kubimo::Error::Custom(format!(
                "Workspace has no python runtime: {name:?}",
                name = workspace.name(),
            ))
        })
}

pub(crate) async fn fetch_workspace_python_runtime(
    ctx: &Context,
    workspace: &Workspace,
) -> kubimo::Result<WorkspacePythonRuntime> {
    if let Some(clone_workspace_name) = workspace.spec.clone_workspace_name.as_ref() {
        let namespace = workspace.require_namespace()?;
        let clone_workspace = ctx
            .api_namespaced::<Workspace>(namespace)
            .get(clone_workspace_name)
            .await?;
        let clone_status = clone_workspace.status.as_ref().ok_or_else(|| {
            kubimo::Error::Custom(format!("Workspace has no status: {clone_workspace_name:?}"))
        })?;
        let python_runtime = clone_status.python_runtime.ok_or_else(|| {
            kubimo::Error::Custom(format!(
                "Workspace has no python runtime: {clone_workspace_name:?}"
            ))
        })?;
        Ok(python_runtime)
    } else {
        Ok(workspace.spec.python_runtime.unwrap_or_default())
    }
}
