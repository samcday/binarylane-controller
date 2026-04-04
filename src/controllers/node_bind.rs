use anyhow::{Context, Result as AnyResult};
use binarylane_client as binarylane;
use k8s_openapi::api::core::v1::Node;
use kube::api::PatchParams;
use kube::runtime::controller::Action;
use kube::{Api, ResourceExt};
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};

use super::{ANNOTATION_ADOPT, LABEL_SERVER_ID, ReconcileContext};

pub async fn reconcile(
    node: Arc<Node>,
    ctx: Arc<ReconcileContext>,
) -> std::result::Result<Action, super::Error> {
    let name = node.name_any();

    let has_adopt = node
        .metadata
        .annotations
        .as_ref()
        .is_some_and(|a| a.contains_key(ANNOTATION_ADOPT));
    if !has_adopt {
        return Ok(Action::await_change());
    }

    let has_server_id = node
        .metadata
        .labels
        .as_ref()
        .is_some_and(|l| l.contains_key(LABEL_SERVER_ID));
    if has_server_id {
        return Ok(Action::await_change());
    }

    match bind_node(&ctx, &name).await {
        Ok(true) => Ok(Action::await_change()),
        Ok(false) => Ok(Action::requeue(Duration::from_secs(30))),
        Err(e) => {
            error!(error = format_args!("{e:#}"), node = %name, "node-bind: binding node");
            Err(e.into())
        }
    }
}

/// Returns `true` if the node was bound, `false` if no server was found yet.
async fn bind_node(ctx: &ReconcileContext, name: &str) -> AnyResult<bool> {
    let nodes_api: Api<Node> = Api::all(ctx.k8s.clone());

    let server = ctx
        .bl
        .get_server_by_hostname(name)
        .await
        .context("looking up server by hostname")?;

    let Some(server) = server else {
        warn!(node = name, "no BinaryLane server found matching hostname");
        return Ok(false);
    };

    info!(
        node = name,
        server_id = server.id,
        "binding node to BinaryLane server"
    );

    // Set providerID and server-id label. Finalizer management is handled by
    // node-deletion, so we don't touch finalizers here to avoid overwriting
    // any existing ones.
    let patch = serde_json::json!({
        "metadata": {
            "labels": {
                LABEL_SERVER_ID: server.id.to_string(),
            },
        },
        "spec": {
            "providerID": binarylane::server_provider_id(server.id),
        },
    });
    nodes_api
        .patch(
            name,
            &PatchParams::apply("binarylane-controller"),
            &kube::api::Patch::Merge(&patch),
        )
        .await
        .context("patching node with provider ID")?;

    Ok(true)
}
