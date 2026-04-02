use anyhow::{Context, Result};
use binarylane_client as binarylane;
use k8s_openapi::api::core::v1::{Node, Service};
use kube::api::PatchParams;
use kube::{Api, Client as KubeClient};
use tracing::{error, info, warn};

use super::ReconcileContext;

const ANNOTATION_LB_ID: &str = "binarylane.com.au/load-balancer-id";
const ANNOTATION_LB_REGION: &str = "binarylane.com.au/load-balancer-region";
const FINALIZER: &str = "binarylane.com.au/load-balancer";

pub async fn reconcile(ctx: &ReconcileContext) {
    let svc_api: Api<Service> = Api::all(ctx.k8s.clone());
    let services = match svc_api.list(&Default::default()).await {
        Ok(list) => list,
        Err(e) => {
            error!(error = %e, "listing services");
            return;
        }
    };

    for svc in &services {
        let spec = match svc.spec.as_ref() {
            Some(s) => s,
            None => continue,
        };
        if spec.type_.as_deref() != Some("LoadBalancer") {
            continue;
        }
        let ns = svc.metadata.namespace.as_deref().unwrap_or("default");
        let name = svc.metadata.name.as_deref().unwrap_or("");
        if let Err(e) = reconcile_service(&ctx.bl, &ctx.k8s, svc, ns, name).await {
            error!(error = %e, service = %format!("{ns}/{name}"), "reconciling service");
        }
    }
}

async fn reconcile_service(
    bl: &binarylane::Client,
    k8s: &KubeClient,
    svc: &Service,
    ns: &str,
    name: &str,
) -> Result<()> {
    let svc_api: Api<Service> = Api::namespaced(k8s.clone(), ns);

    // Handle deletion
    if svc.metadata.deletion_timestamp.is_some() {
        return handle_deletion(bl, &svc_api, svc, ns, name).await;
    }

    // Ensure finalizer
    let has_finalizer = svc
        .metadata
        .finalizers
        .as_ref()
        .is_some_and(|f| f.iter().any(|x| x == FINALIZER));
    if !has_finalizer {
        let mut finalizers: Vec<String> = svc.metadata.finalizers.clone().unwrap_or_default();
        finalizers.push(FINALIZER.to_string());
        let patch = serde_json::json!({
            "metadata": { "finalizers": finalizers }
        });
        svc_api
            .patch(
                name,
                &PatchParams::apply("binarylane-controller"),
                &kube::api::Patch::Merge(&patch),
            )
            .await
            .context("adding finalizer")?;
    }

    // Get ready node server IDs
    let node_ids = get_ready_node_server_ids(k8s).await?;

    // Build forwarding rules
    let spec = svc.spec.as_ref().unwrap();
    let ports = spec.ports.as_deref().unwrap_or(&[]);
    let rules: Vec<binarylane::ForwardingRule> = ports
        .iter()
        .map(|p| {
            let proto = p.protocol.as_deref().unwrap_or("TCP").to_lowercase();
            binarylane::ForwardingRule {
                entry_protocol: proto.clone(),
                entry_port: p.port,
                target_protocol: proto,
                target_port: p.node_port.unwrap_or(p.port),
            }
        })
        .collect();

    let first_node_port = ports.first().and_then(|p| p.node_port).unwrap_or(80);
    let health_check = binarylane::HealthCheck {
        protocol: "tcp".to_string(),
        port: first_node_port,
        path: None,
        check_interval_seconds: 10,
        response_timeout_seconds: 5,
        unhealthy_threshold: 3,
        healthy_threshold: 5,
    };

    let annotations = svc.metadata.annotations.as_ref();
    let lb_id_str = annotations.and_then(|a| a.get(ANNOTATION_LB_ID));
    let region = annotations
        .and_then(|a| a.get(ANNOTATION_LB_REGION))
        .map(|s| s.as_str())
        .unwrap_or("syd");

    if let Some(lb_id_str) = lb_id_str {
        let lb_id: i64 = lb_id_str.parse().context("parsing LB ID annotation")?;
        let existing = bl
            .get_load_balancer(lb_id)
            .await
            .context("getting load balancer")?;
        let Some(existing) = existing else {
            warn!(lb_id, service = %format!("{ns}/{name}"), "load balancer not found, recreating");
            return create_load_balancer(
                bl, &svc_api, ns, name, region, rules, health_check, node_ids,
            )
            .await;
        };

        let desired_name = lb_name(ns, name);
        let mut needs_update = false;
        needs_update |= existing.name != desired_name;
        needs_update |= {
            let mut existing_rules = existing.forwarding_rules.clone();
            let mut desired_rules = rules.clone();
            existing_rules.sort_by_key(|r| r.entry_port);
            desired_rules.sort_by_key(|r| r.entry_port);
            existing_rules != desired_rules
        };
        needs_update |= {
            let mut existing_ids = existing.server_ids.clone();
            let mut desired_ids = node_ids.clone();
            existing_ids.sort();
            desired_ids.sort();
            existing_ids != desired_ids
        };

        if !needs_update {
            return update_service_status(&svc_api, name, &existing).await;
        }

        let lb = bl
            .update_load_balancer(
                lb_id,
                binarylane::UpdateLoadBalancerRequest {
                    name: desired_name,
                    forwarding_rules: rules,
                    health_check: Some(health_check),
                    server_ids: node_ids,
                },
            )
            .await
            .context("updating load balancer")?;
        return update_service_status(&svc_api, name, &lb).await;
    }

    create_load_balancer(bl, &svc_api, ns, name, region, rules, health_check, node_ids).await
}

#[allow(clippy::too_many_arguments)]
async fn create_load_balancer(
    bl: &binarylane::Client,
    svc_api: &Api<Service>,
    ns: &str,
    name: &str,
    region: &str,
    rules: Vec<binarylane::ForwardingRule>,
    health_check: binarylane::HealthCheck,
    node_ids: Vec<i64>,
) -> Result<()> {
    info!(service = %format!("{ns}/{name}"), region, "creating load balancer");
    let lb = bl
        .create_load_balancer(binarylane::CreateLoadBalancerRequest {
            name: lb_name(ns, name),
            region: region.to_string(),
            forwarding_rules: rules,
            health_check: Some(health_check),
            server_ids: node_ids,
        })
        .await
        .context("creating load balancer")?;

    let patch = serde_json::json!({
        "metadata": {
            "annotations": {
                ANNOTATION_LB_ID: lb.id.to_string()
            }
        }
    });
    svc_api
        .patch(
            name,
            &PatchParams::apply("binarylane-controller"),
            &kube::api::Patch::Merge(&patch),
        )
        .await
        .context("setting LB ID annotation")?;

    update_service_status(svc_api, name, &lb).await
}

async fn update_service_status(
    svc_api: &Api<Service>,
    name: &str,
    lb: &binarylane::LoadBalancer,
) -> Result<()> {
    if lb.ip.is_empty() {
        return Ok(());
    }
    let patch = serde_json::json!({
        "status": {
            "loadBalancer": {
                "ingress": [{ "ip": lb.ip }]
            }
        }
    });
    svc_api
        .patch_status(
            name,
            &PatchParams::apply("binarylane-controller"),
            &kube::api::Patch::Merge(&patch),
        )
        .await
        .context("updating service status")?;
    info!(service = name, ip = %lb.ip, "updated service ingress");
    Ok(())
}

async fn handle_deletion(
    bl: &binarylane::Client,
    svc_api: &Api<Service>,
    svc: &Service,
    ns: &str,
    name: &str,
) -> Result<()> {
    let has_finalizer = svc
        .metadata
        .finalizers
        .as_ref()
        .is_some_and(|f| f.iter().any(|x| x == FINALIZER));
    if !has_finalizer {
        return Ok(());
    }

    if let Some(lb_id_str) = svc
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(ANNOTATION_LB_ID))
    {
        let lb_id: i64 = lb_id_str.parse().context("parsing LB ID for deletion")?;
        info!(lb_id, service = %format!("{ns}/{name}"), "deleting load balancer");
        bl.delete_load_balancer(lb_id)
            .await
            .context("deleting load balancer")?;
    }

    let finalizers: Vec<String> = svc
        .metadata
        .finalizers
        .as_ref()
        .map(|f| {
            f.iter()
                .filter(|x| x.as_str() != FINALIZER)
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let patch = serde_json::json!({
        "metadata": { "finalizers": finalizers }
    });
    svc_api
        .patch(
            name,
            &PatchParams::apply("binarylane-controller"),
            &kube::api::Patch::Merge(&patch),
        )
        .await
        .context("removing finalizer")?;
    Ok(())
}

async fn get_ready_node_server_ids(k8s: &KubeClient) -> Result<Vec<i64>> {
    let nodes_api: Api<Node> = Api::all(k8s.clone());
    let nodes = nodes_api
        .list(&Default::default())
        .await
        .context("listing nodes")?;
    let mut ids = Vec::new();
    for node in &nodes {
        let provider_id = node
            .spec
            .as_ref()
            .and_then(|s| s.provider_id.as_deref())
            .unwrap_or("");
        let Some(server_id) = binarylane::parse_provider_id(provider_id) else {
            continue;
        };
        let is_ready = node
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .map(|conds| {
                conds
                    .iter()
                    .any(|c| c.type_ == "Ready" && c.status == "True")
            })
            .unwrap_or(false);
        if is_ready {
            ids.push(server_id);
        }
    }
    Ok(ids)
}

fn lb_name(ns: &str, name: &str) -> String {
    format!("k8s-{ns}-{name}")
}
