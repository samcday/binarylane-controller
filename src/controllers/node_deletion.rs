use anyhow::{Context, Result as AnyResult};
use binarylane_client as binarylane;
use k8s_openapi::api::core::v1::{Node, Secret};
use kube::api::PatchParams;
use kube::runtime::controller::Action;
use kube::{Api, ResourceExt};
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info};

use super::{FINALIZER, ReconcileContext, node_password_secret_name, user_data_secret_name};

pub async fn reconcile(
    node: Arc<Node>,
    ctx: Arc<ReconcileContext>,
) -> std::result::Result<Action, super::Error> {
    let name = node.name_any();
    let provider_id = node
        .spec
        .as_ref()
        .and_then(|s| s.provider_id.as_deref())
        .unwrap_or("");
    let server_id = binarylane::parse_provider_id(provider_id);

    let has_finalizer = node
        .metadata
        .finalizers
        .as_ref()
        .is_some_and(|f| f.iter().any(|f| f == FINALIZER));
    let has_provision_labels = node.metadata.labels.as_ref().is_some_and(|l| {
        l.contains_key(super::LABEL_SIZE)
            || l.contains_key(super::LABEL_REGION)
            || l.contains_key(super::LABEL_IMAGE)
    });

    if server_id.is_none() && !has_finalizer && !has_provision_labels {
        return Ok(Action::await_change());
    }

    if let Err(e) = reconcile_node(&ctx, &node, &name, server_id).await {
        error!(error = %e, node = %name, ?server_id, "node-deletion: reconciling node");
        return Err(e.into());
    }

    Ok(Action::requeue(Duration::from_secs(300)))
}

async fn reconcile_node(
    ctx: &ReconcileContext,
    node: &Node,
    name: &str,
    server_id: Option<i64>,
) -> AnyResult<()> {
    let nodes_api: Api<Node> = Api::all(ctx.k8s.clone());
    let secrets_api: Api<Secret> = Api::namespaced(ctx.k8s.clone(), &ctx.secret_namespace);

    let has_finalizer = node
        .metadata
        .finalizers
        .as_ref()
        .is_some_and(|f| f.iter().any(|f| f == FINALIZER));

    // Node is being deleted and has our finalizer: delete the BL server (if
    // bound), clean up secrets, then remove the finalizer so the node can be GC'd.
    if node.metadata.deletion_timestamp.is_some() && has_finalizer {
        if let Some(sid) = server_id {
            info!(
                node = name,
                server_id = sid,
                "node deleted, deleting BinaryLane server"
            );
            ctx.bl.delete_server(sid).await.context("deleting server")?;
        }

        delete_secret_if_exists(&secrets_api, &node_password_secret_name(name)).await?;
        delete_secret_if_exists(&secrets_api, &user_data_secret_name(name)).await?;

        let patch = serde_json::json!({
            "metadata": {
                "finalizers": node.metadata.finalizers.as_ref()
                    .map(|f| f.iter().filter(|f| f.as_str() != FINALIZER).collect::<Vec<_>>())
                    .unwrap_or_default()
            }
        });
        nodes_api
            .patch(
                name,
                &PatchParams::apply("binarylane-controller"),
                &kube::api::Patch::Merge(&patch),
            )
            .await
            .context("removing finalizer")?;
        return Ok(());
    }

    // If server is bound but no longer exists, clean up secrets then delete the K8s node.
    if let Some(sid) = server_id {
        let server = ctx.bl.get_server(sid).await.context("getting server")?;
        if server.is_none() {
            info!(
                node = name,
                server_id = sid,
                "server deleted, removing node"
            );
            delete_secret_if_exists(&secrets_api, &node_password_secret_name(name)).await?;
            delete_secret_if_exists(&secrets_api, &user_data_secret_name(name)).await?;
            nodes_api
                .delete(name, &Default::default())
                .await
                .context("deleting node")?;
            return Ok(());
        }
    }

    // Ensure finalizer is present for bound nodes and provision candidates.
    let has_provision_labels = node.metadata.labels.as_ref().is_some_and(|l| {
        l.contains_key(super::LABEL_SIZE)
            || l.contains_key(super::LABEL_REGION)
            || l.contains_key(super::LABEL_IMAGE)
    });
    let has_adopt = node
        .metadata
        .annotations
        .as_ref()
        .is_some_and(|a| a.contains_key(super::ANNOTATION_ADOPT));
    if (server_id.is_some() || has_provision_labels || has_adopt) && !has_finalizer {
        let mut finalizers: Vec<String> = node.metadata.finalizers.clone().unwrap_or_default();
        finalizers.push(FINALIZER.to_string());
        let patch = serde_json::json!({
            "metadata": { "finalizers": finalizers }
        });
        nodes_api
            .patch(
                name,
                &PatchParams::apply("binarylane-controller"),
                &kube::api::Patch::Merge(&patch),
            )
            .await
            .context("adding finalizer")?;
    }

    // Tether password secret to node via ownerReference (secret is created
    // before the node exists, so we set the ownerRef here on first reconciliation).
    if let Some(node_uid) = &node.metadata.uid {
        tether_secret_to_node(
            &secrets_api,
            &node_password_secret_name(name),
            name,
            node_uid,
        )
        .await;
        tether_secret_to_node(&secrets_api, &user_data_secret_name(name), name, node_uid).await;
    }

    Ok(())
}

async fn delete_secret_if_exists(secrets_api: &Api<Secret>, name: &str) -> AnyResult<()> {
    match secrets_api.delete(name, &Default::default()).await {
        Ok(_) => {
            info!(secret = name, "deleted secret");
        }
        Err(kube::Error::Api(err)) if err.code == 404 => {}
        Err(e) => {
            return Err(e).with_context(|| format!("deleting secret {name}"));
        }
    }
    Ok(())
}

async fn tether_secret_to_node(
    secrets_api: &Api<Secret>,
    secret_name: &str,
    node_name: &str,
    node_uid: &str,
) {
    let secret = match secrets_api.get_opt(secret_name).await {
        Ok(Some(s)) => s,
        _ => return,
    };
    let has_owner_ref = secret
        .metadata
        .owner_references
        .as_ref()
        .is_some_and(|refs| refs.iter().any(|r| r.kind == "Node" && r.name == node_name));
    if has_owner_ref {
        return;
    }
    let patch = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": secret_name,
            "ownerReferences": [{
                "apiVersion": "v1",
                "kind": "Node",
                "name": node_name,
                "uid": node_uid,
                "blockOwnerDeletion": false,
            }]
        }
    });
    if let Err(e) = secrets_api
        .patch(
            secret_name,
            &PatchParams::apply("binarylane-controller-node").force(),
            &kube::api::Patch::Apply(&patch),
        )
        .await
    {
        error!(
            node = node_name,
            secret = secret_name,
            error = %e,
            "setting ownerReference on secret"
        );
    }
}
