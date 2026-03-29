use std::collections::BTreeMap;

use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Node;
use kube::{Api, Client as KubeClient, api::PatchParams};
use tracing::{error, info};

use crate::binarylane::{self, Client as BlClient};

const UNINITIALIZED_TAINT: &str = "node.cloudprovider.kubernetes.io/uninitialized";
pub const FINALIZER: &str = "blc.samcday.com/server-cleanup";

pub async fn reconcile(bl: &BlClient, k8s: &KubeClient) {
    let nodes_api: Api<Node> = Api::all(k8s.clone());
    let nodes = match nodes_api.list(&Default::default()).await {
        Ok(list) => list,
        Err(e) => {
            error!(error = %e, "listing nodes");
            return;
        }
    };

    for node in &nodes {
        let name = node.metadata.name.as_deref().unwrap_or("");
        let provider_id = node
            .spec
            .as_ref()
            .and_then(|s| s.provider_id.as_deref())
            .unwrap_or("");
        let Some(server_id) = binarylane::parse_provider_id(provider_id) else {
            continue;
        };
        if let Err(e) = reconcile_node(bl, &nodes_api, node, name, server_id).await {
            error!(error = %e, node = name, server_id, "reconciling node");
        }
    }
}

async fn reconcile_node(
    bl: &BlClient,
    nodes_api: &Api<Node>,
    node: &Node,
    name: &str,
    server_id: i64,
) -> Result<()> {
    let has_finalizer = node
        .metadata
        .finalizers
        .as_ref()
        .is_some_and(|f| f.iter().any(|f| f == FINALIZER));

    // Node is being deleted and has our finalizer: delete the BL server, then
    // remove the finalizer so the node can be garbage-collected.
    if node.metadata.deletion_timestamp.is_some() && has_finalizer {
        info!(
            node = name,
            server_id, "node deleted, deleting BinaryLane server"
        );
        bl.delete_server(server_id)
            .await
            .context("deleting server")?;
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

    let server = bl.get_server(server_id).await.context("getting server")?;

    let Some(server) = server else {
        info!(node = name, server_id, "server deleted, removing node");
        nodes_api
            .delete(name, &Default::default())
            .await
            .context("deleting node")?;
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

    // Patch spec/metadata if labels or taints changed
    if needs_update {
        let new_taints: Vec<_> = taints
            .into_iter()
            .filter(|t| t.key != UNINITIALIZED_TAINT)
            .collect();

        let patch = serde_json::json!({
            "metadata": {
                "labels": labels,
            },
            "spec": {
                "taints": new_taints,
            },
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
