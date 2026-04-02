use anyhow::{Context, Result};
use binarylane_client as binarylane;
use k8s_openapi::api::core::v1::Node;
use kube::Api;
use kube::api::PatchParams;
use tracing::{error, info, warn};

use super::{ANNOTATION_ADOPT, LABEL_SERVER_ID, ReconcileContext};

pub async fn reconcile(ctx: &ReconcileContext) {
    let nodes_api: Api<Node> = Api::all(ctx.k8s.clone());
    let nodes = match nodes_api.list(&Default::default()).await {
        Ok(list) => list,
        Err(e) => {
            error!(error = %e, "node-bind: listing nodes");
            return;
        }
    };

    for node in &nodes {
        let Some(name) = node.metadata.name.as_deref() else {
            continue;
        };

        // Only process nodes with the adopt annotation.
        let has_adopt = node
            .metadata
            .annotations
            .as_ref()
            .is_some_and(|a| a.contains_key(ANNOTATION_ADOPT));
        if !has_adopt {
            continue;
        }

        // Skip nodes already bound to a server.
        let has_server_id = node
            .metadata
            .labels
            .as_ref()
            .is_some_and(|l| l.contains_key(LABEL_SERVER_ID));
        if has_server_id {
            continue;
        }

        if let Err(e) = bind_node(ctx, &nodes_api, name).await {
            error!(error = %e, node = name, "node-bind: binding node");
        }
    }
}

async fn bind_node(ctx: &ReconcileContext, nodes_api: &Api<Node>, name: &str) -> Result<()> {
    let server = ctx
        .bl
        .get_server_by_hostname(name)
        .await
        .context("looking up server by hostname")?;

    let Some(server) = server else {
        warn!(node = name, "no BinaryLane server found matching hostname");
        return Ok(());
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

    Ok(())
}
