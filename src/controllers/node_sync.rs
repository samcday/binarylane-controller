use std::collections::BTreeMap;

use anyhow::{Context, Result};
use binarylane_client as binarylane;
use k8s_openapi::api::core::v1::Node;
use kube::api::PatchParams;
use kube::Api;
use tracing::{error, info};

use super::{ReconcileContext, UNINITIALIZED_TAINT};

pub async fn reconcile(ctx: &ReconcileContext) {
    let nodes_api: Api<Node> = Api::all(ctx.k8s.clone());
    let nodes = match nodes_api.list(&Default::default()).await {
        Ok(list) => list,
        Err(e) => {
            error!(error = %e, "node-sync: listing nodes");
            return;
        }
    };

    for node in &nodes {
        let Some(name) = node.metadata.name.as_deref() else {
            continue;
        };
        if node.metadata.deletion_timestamp.is_some() {
            continue;
        }
        let provider_id = node
            .spec
            .as_ref()
            .and_then(|s| s.provider_id.as_deref())
            .unwrap_or("");
        let Some(server_id) = binarylane::parse_provider_id(provider_id) else {
            continue;
        };
        if let Err(e) = reconcile_node(ctx, &nodes_api, node, name, server_id).await {
            error!(error = %e, node = name, server_id, "node-sync: reconciling node");
        }
    }
}

async fn reconcile_node(
    ctx: &ReconcileContext,
    nodes_api: &Api<Node>,
    node: &Node,
    name: &str,
    server_id: i64,
) -> Result<()> {
    let server = ctx
        .bl
        .get_server(server_id)
        .await
        .context("getting server")?;
    let Some(server) = server else {
        // Server gone — node-deletion will handle this.
        return Ok(());
    };

    let mut needs_update = false;
    let mut needs_status_update = false;

    // Build desired addresses.
    // BL reports phantom private IPs even without a VPC — skip them.
    let mut desired_addresses = Vec::new();
    for net in &server.networks.v4 {
        let addr_type = match net.net_type.as_str() {
            "public" => "ExternalIP",
            "private" if server.vpc_id.is_some() => "InternalIP",
            "private" => continue,
            _ => continue,
        };
        desired_addresses.push(serde_json::json!({
            "type": addr_type,
            "address": net.ip_address,
        }));
    }
    desired_addresses.push(serde_json::json!({
        "type": "Hostname",
        "address": server.name,
    }));

    // Build desired labels
    let mut labels: BTreeMap<String, String> = node.metadata.labels.clone().unwrap_or_default();
    let desired_labels: [(&str, &str); 3] = [
        ("node.kubernetes.io/instance-type", &server.size_slug),
        ("topology.kubernetes.io/region", &server.region.slug),
        (
            "node.kubernetes.io/cloud-provider",
            binarylane::PROVIDER_NAME,
        ),
    ];
    for (k, v) in &desired_labels {
        if labels.get(*k).map(String::as_str) != Some(*v) {
            labels.insert(k.to_string(), v.to_string());
            needs_update = true;
        }
    }

    // Check if taint needs removal
    let taints = node
        .spec
        .as_ref()
        .and_then(|s| s.taints.as_ref())
        .cloned()
        .unwrap_or_default();
    let has_uninit_taint = taints.iter().any(|t| t.key == UNINITIALIZED_TAINT);
    if has_uninit_taint {
        needs_update = true;
        info!(node = name, "removing uninitialized taint");
    }

    // Check addresses
    let current_addresses = node
        .status
        .as_ref()
        .map(|s| {
            s.addresses
                .as_ref()
                .map(|addrs| {
                    addrs
                        .iter()
                        .map(|a| {
                            serde_json::json!({
                                "type": a.type_,
                                "address": a.address,
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .unwrap_or_default();
    if current_addresses != desired_addresses {
        needs_status_update = true;
    }

    if !needs_update && !needs_status_update {
        return Ok(());
    }

    if needs_update {
        let new_taints: Vec<_> = taints
            .into_iter()
            .filter(|t| t.key != UNINITIALIZED_TAINT)
            .collect();
        let patch = serde_json::json!({
            "metadata": { "labels": labels },
            "spec": { "taints": new_taints },
        });
        nodes_api
            .patch(
                name,
                &PatchParams::apply("binarylane-controller"),
                &kube::api::Patch::Merge(&patch),
            )
            .await
            .context("patching node")?;
    }

    if needs_status_update {
        nodes_api
            .patch_status(
                name,
                &PatchParams::apply("binarylane-controller"),
                &kube::api::Patch::Merge(&serde_json::json!({
                    "status": { "addresses": desired_addresses }
                })),
            )
            .await
            .context("patching node status")?;
    }

    info!(node = name, server_id, "reconciled node");
    Ok(())
}
