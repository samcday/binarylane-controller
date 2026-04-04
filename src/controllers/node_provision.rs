use anyhow::{Context, Result as AnyResult};
use binarylane_client as binarylane;
use k8s_openapi::api::core::v1::{Event, EventSource, Node, ObjectReference, Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::api::PatchParams;
use kube::runtime::controller::Action;
use kube::{Api, ResourceExt};
use std::sync::Arc;
use tracing::{error, info, warn};

use super::{
    LABEL_IMAGE, LABEL_REGION, LABEL_SERVER_ID, LABEL_SIZE, ReconcileContext,
    node_password_secret_name, user_data_secret_name,
};

const ANNOTATION_PROVISION_FAILED: &str = "bl.samcday.com/provision-failed-config";

/// Stable fingerprint of the provision-relevant label values.
fn config_hash(labels: Option<&std::collections::BTreeMap<String, String>>) -> String {
    [LABEL_SIZE, LABEL_REGION, LABEL_IMAGE]
        .iter()
        .map(|k| {
            labels
                .and_then(|l| l.get(*k))
                .map(|v| v.as_str())
                .unwrap_or("")
        })
        .collect::<Vec<_>>()
        .join("|")
}

pub async fn reconcile(
    node: Arc<Node>,
    ctx: Arc<ReconcileContext>,
) -> std::result::Result<Action, super::Error> {
    let name = node.name_any();
    let node_uid = node.metadata.uid.clone();
    let labels = node.metadata.labels.as_ref();

    if labels.is_some_and(|l| l.contains_key(LABEL_SERVER_ID)) {
        return Ok(Action::await_change());
    }

    if node.metadata.deletion_timestamp.is_some() {
        return Ok(Action::await_change());
    }

    let has_any = labels.is_some_and(|l| {
        l.contains_key(LABEL_SIZE) || l.contains_key(LABEL_REGION) || l.contains_key(LABEL_IMAGE)
    });
    if !has_any {
        return Ok(Action::await_change());
    }

    let hash = config_hash(labels);
    let already_failed = node
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(ANNOTATION_PROVISION_FAILED))
        .is_some_and(|v| *v == hash);
    if already_failed {
        return Ok(Action::await_change());
    }

    let nodes_api: Api<Node> = Api::all(ctx.k8s.clone());

    let size = labels.and_then(|l| l.get(LABEL_SIZE));
    let region = labels.and_then(|l| l.get(LABEL_REGION));
    let image = labels.and_then(|l| l.get(LABEL_IMAGE));

    let mut missing = Vec::new();
    if size.is_none() {
        missing.push(LABEL_SIZE);
    }
    if region.is_none() {
        missing.push(LABEL_REGION);
    }
    if image.is_none() {
        missing.push(LABEL_IMAGE);
    }
    if !missing.is_empty() {
        let msg = format!("missing required labels: {}", missing.join(", "));
        set_provision_failed(
            &nodes_api,
            &ctx.k8s,
            &name,
            node_uid,
            &hash,
            "InvalidConfig",
            &msg,
        )
        .await;
        return Ok(Action::await_change());
    }

    let size = size.cloned().unwrap_or_default();
    let region = region.cloned().unwrap_or_default();
    let image = image.cloned().unwrap_or_default();

    let catalog = ctx.bl_catalog().await?;

    if !catalog.sizes.iter().any(|s| s.slug == size) {
        let msg = format!(
            "unknown size '{}' (available: {})",
            size,
            catalog
                .sizes
                .iter()
                .map(|s| s.slug.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        set_provision_failed(
            &nodes_api,
            &ctx.k8s,
            &name,
            node_uid,
            &hash,
            "InvalidConfig",
            &msg,
        )
        .await;
        return Ok(Action::await_change());
    }

    if !catalog.regions.iter().any(|r| r.slug == region) {
        let msg = format!(
            "unknown region '{}' (available: {})",
            region,
            catalog
                .regions
                .iter()
                .map(|r| r.slug.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        set_provision_failed(
            &nodes_api,
            &ctx.k8s,
            &name,
            node_uid,
            &hash,
            "InvalidConfig",
            &msg,
        )
        .await;
        return Ok(Action::await_change());
    }

    if !catalog
        .images
        .iter()
        .any(|i| i.slug.as_deref() == Some(image.as_str()))
    {
        let msg = format!(
            "unknown image '{}' (available: {})",
            image,
            catalog
                .images
                .iter()
                .filter_map(|i| i.slug.as_deref())
                .collect::<Vec<_>>()
                .join(", ")
        );
        set_provision_failed(
            &nodes_api,
            &ctx.k8s,
            &name,
            node_uid,
            &hash,
            "InvalidConfig",
            &msg,
        )
        .await;
        return Ok(Action::await_change());
    }

    if let Err(e) = provision_node(
        &ctx,
        &nodes_api,
        &name,
        size,
        region,
        image,
        node_uid.clone(),
    )
    .await
    {
        let msg = format!("server creation failed: {e:#}");
        error!(error = format_args!("{e:#}"), node = %name, "node-provision: provisioning failed");
        emit_event(&ctx.k8s, &name, node_uid, "Warning", "ProvisionError", &msg).await;
        return Err(e.into());
    }

    clear_provision_failed(&nodes_api, &name).await;
    Ok(Action::await_change())
}

async fn set_provision_failed(
    nodes_api: &Api<Node>,
    k8s: &kube::Client,
    name: &str,
    node_uid: Option<String>,
    config_hash: &str,
    reason: &str,
    message: &str,
) {
    warn!(
        node = name,
        reason, message, "node-provision: provisioning failed"
    );

    let patch = serde_json::json!({
        "metadata": {
            "annotations": {
                ANNOTATION_PROVISION_FAILED: config_hash,
            }
        }
    });
    if let Err(e) = nodes_api
        .patch(
            name,
            &PatchParams::apply("binarylane-controller"),
            &kube::api::Patch::Merge(&patch),
        )
        .await
    {
        warn!(node = name, error = %e, "failed to set provision-failed annotation");
    }

    emit_event(k8s, name, node_uid, "Warning", "ProvisionFailed", message).await;
}

async fn clear_provision_failed(nodes_api: &Api<Node>, name: &str) {
    // Remove the failed config annotation.
    let patch = serde_json::json!({
        "metadata": {
            "annotations": {
                ANNOTATION_PROVISION_FAILED: null,
            }
        }
    });
    if let Err(e) = nodes_api
        .patch(
            name,
            &PatchParams::apply("binarylane-controller"),
            &kube::api::Patch::Merge(&patch),
        )
        .await
    {
        warn!(node = name, error = %e, "failed to clear provision-failed annotation");
    }
}

async fn emit_event(
    k8s: &kube::Client,
    node_name: &str,
    node_uid: Option<String>,
    event_type: &str,
    reason: &str,
    message: &str,
) {
    let events_api: Api<Event> = Api::namespaced(k8s.clone(), "default");
    let now = Time(k8s_openapi::chrono::Utc::now());
    let event = Event {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            generate_name: Some("binarylane-controller-".to_string()),
            namespace: Some("default".to_string()),
            ..Default::default()
        },
        involved_object: ObjectReference {
            api_version: Some("v1".to_string()),
            kind: Some("Node".to_string()),
            name: Some(node_name.to_string()),
            uid: node_uid,
            ..Default::default()
        },
        reason: Some(reason.to_string()),
        message: Some(message.to_string()),
        type_: Some(event_type.to_string()),
        source: Some(EventSource {
            component: Some("binarylane-controller".to_string()),
            ..Default::default()
        }),
        first_timestamp: Some(now.clone()),
        last_timestamp: Some(now),
        count: Some(1),
        action: Some("Provision".to_string()),
        ..Default::default()
    };
    if let Err(e) = events_api.create(&Default::default(), &event).await {
        warn!(node = node_name, error = %e, reason, "failed to emit event");
    }
}

async fn provision_node(
    ctx: &ReconcileContext,
    nodes_api: &Api<Node>,
    name: &str,
    size: String,
    region: String,
    image: String,
    node_uid: Option<String>,
) -> AnyResult<()> {
    let secrets_api: Api<Secret> = Api::namespaced(ctx.k8s.clone(), &ctx.secret_namespace);

    // Ensure finalizer is present before creating any external resources.
    let current = nodes_api
        .get(name)
        .await
        .context("getting node for finalizer")?;
    let has_finalizer = current
        .metadata
        .finalizers
        .as_ref()
        .is_some_and(|f| f.iter().any(|f| f == super::FINALIZER));
    if !has_finalizer {
        let mut finalizers = current.metadata.finalizers.unwrap_or_default();
        finalizers.push(super::FINALIZER.to_string());
        let patch = serde_json::json!({ "metadata": { "finalizers": finalizers } });
        nodes_api
            .patch(
                name,
                &PatchParams::apply("binarylane-controller"),
                &kube::api::Patch::Merge(&patch),
            )
            .await
            .context("adding finalizer")?;
    }

    // Read or generate password. If no secret exists, generate a password and
    // persist it so it's not lost if server creation fails mid-flight.
    let password_secret_name = node_password_secret_name(name);
    let password = match secrets_api.get_opt(&password_secret_name).await {
        Ok(Some(secret)) => secret
            .data
            .as_ref()
            .and_then(|d| d.get("password"))
            .map(|v| String::from_utf8_lossy(&v.0).to_string()),
        _ => None,
    };
    let password = match password {
        Some(p) => p,
        None => {
            let p = binarylane::generate_server_password();
            let patch = serde_json::json!({
                "apiVersion": "v1",
                "kind": "Secret",
                "metadata": {
                    "name": password_secret_name,
                    "labels": {
                        "app.kubernetes.io/managed-by": "binarylane-controller",
                    },
                },
                "type": "Opaque",
                "stringData": { "password": &p },
            });
            secrets_api
                .patch(
                    &password_secret_name,
                    &PatchParams::apply("binarylane-controller").force(),
                    &kube::api::Patch::Apply(&patch),
                )
                .await
                .context("creating password secret")?;
            info!(node = name, "generated password secret");
            p
        }
    };

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

    // Check for an existing server first to avoid creating duplicates on retry.
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
                password: Some(password),
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

    emit_event(
        &ctx.k8s,
        name,
        node_uid,
        "Normal",
        "ServerCreated",
        &format!(
            "Created BinaryLane server {} ({}, {})",
            srv.id, srv.size_slug, srv.region.slug
        ),
    )
    .await;

    info!(node = name, server_id = srv.id, "provisioned server");
    Ok(())
}
