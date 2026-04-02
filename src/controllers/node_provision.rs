use anyhow::{Context, Result};
use binarylane_client as binarylane;
use k8s_openapi::api::core::v1::{Node, Secret};
use kube::Api;
use kube::api::PatchParams;
use tracing::{error, info};

use super::{
    LABEL_IMAGE, LABEL_REGION, LABEL_SERVER_ID, LABEL_SIZE, ReconcileContext,
    node_password_secret_name, user_data_secret_name,
};

pub async fn reconcile(ctx: &ReconcileContext) {
    let nodes_api: Api<Node> = Api::all(ctx.k8s.clone());
    let nodes = match nodes_api.list(&Default::default()).await {
        Ok(list) => list,
        Err(e) => {
            error!(error = %e, "node-provision: listing nodes");
            return;
        }
    };

    for node in &nodes {
        let Some(name) = node.metadata.name.as_deref() else {
            continue;
        };
        let labels = node.metadata.labels.as_ref();

        // Skip nodes already bound to a server.
        if labels.is_some_and(|l| l.contains_key(LABEL_SERVER_ID)) {
            continue;
        }

        // Skip nodes being deleted.
        if node.metadata.deletion_timestamp.is_some() {
            continue;
        }

        // Need all three provision labels.
        let Some(size) = labels.and_then(|l| l.get(LABEL_SIZE)) else {
            continue;
        };
        let Some(region) = labels.and_then(|l| l.get(LABEL_REGION)) else {
            continue;
        };
        let Some(image) = labels.and_then(|l| l.get(LABEL_IMAGE)) else {
            continue;
        };

        if let Err(e) = provision_node(
            ctx,
            &nodes_api,
            name,
            size.clone(),
            region.clone(),
            image.clone(),
        )
        .await
        {
            error!(error = %e, node = name, "node-provision: provisioning node");
        }
    }
}

async fn provision_node(
    ctx: &ReconcileContext,
    nodes_api: &Api<Node>,
    name: &str,
    size: String,
    region: String,
    image: String,
) -> Result<()> {
    let secrets_api: Api<Secret> = Api::namespaced(ctx.k8s.clone(), &ctx.secret_namespace);

    // Read password from secret.
    let password_secret_name = node_password_secret_name(name);
    let password_secret = secrets_api
        .get_opt(&password_secret_name)
        .await
        .context("getting password secret")?;
    let password = password_secret
        .as_ref()
        .and_then(|s| s.data.as_ref())
        .and_then(|d| d.get("password"))
        .map(|v| String::from_utf8_lossy(&v.0).to_string());

    // Read user-data from secret.
    let user_data_secret_name = user_data_secret_name(name);
    let user_data_secret = secrets_api
        .get_opt(&user_data_secret_name)
        .await
        .context("getting user-data secret")?;
    let user_data = user_data_secret
        .as_ref()
        .and_then(|s| s.data.as_ref())
        .and_then(|d| d.get("user-data"))
        .map(|v| String::from_utf8_lossy(&v.0).to_string());

    // Check for an existing server first to avoid creating duplicates on retry
    // (e.g. if a previous reconcile created the server but crashed before
    // patching the Node with the server-id label).
    let srv = if let Some(existing) = ctx
        .bl
        .get_server_by_hostname(name)
        .await
        .context("checking for existing server")?
    {
        info!(
            node = name,
            server_id = existing.id,
            "reusing existing BinaryLane server"
        );
        existing
    } else {
        info!(node = name, %size, %region, %image, "creating BinaryLane server");
        ctx.bl
            .create_server(binarylane::CreateServerRequest {
                name: name.to_string(),
                size,
                image,
                region,
                user_data,
                ssh_keys: None,
                password,
            })
            .await
            .context("creating server")?
    };

    // Set providerID and server-id label on the node.
    let patch = serde_json::json!({
        "metadata": {
            "labels": {
                LABEL_SERVER_ID: srv.id.to_string(),
            },
        },
        "spec": {
            "providerID": binarylane::server_provider_id(srv.id),
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

    info!(node = name, server_id = srv.id, "provisioned server");
    Ok(())
}
