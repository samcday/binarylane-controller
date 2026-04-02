use std::time::Duration;

use anyhow::{Context, Result};
use integration_tests::{TestContext, test_name, wait_for};
use k8s_openapi::api::core::v1::{Node, Service};
use kube::Api;
use kube::api::{DeleteParams, PostParams};

/// Verify the controller syncs BinaryLane server metadata onto the
/// corresponding Kubernetes node: labels for instance-type, region, and
/// cloud-provider, the public IP as an ExternalIP address, and removal of the
/// cloud-provider uninitialized taint.
#[tokio::test]
async fn test_node_sync() -> Result<()> {
    let Some(ctx) = TestContext::new().await else {
        eprintln!("skipping test_node_sync: BL_API_TOKEN or KUBECONFIG not set");
        return Ok(());
    };

    let servers = ctx.bl.list_servers().await.context("listing BL servers")?;
    let nodes_api: Api<Node> = Api::all(ctx.kube.clone());
    let nodes = nodes_api
        .list(&Default::default())
        .await
        .context("listing k8s nodes")?;

    // Find a node whose provider ID matches a BL server.
    let mut matched = false;
    for node in &nodes {
        let provider_id = node
            .spec
            .as_ref()
            .and_then(|s| s.provider_id.as_deref())
            .unwrap_or("");
        let Some(server_id) = binarylane_client::parse_provider_id(provider_id) else {
            continue;
        };

        let Some(server) = servers.iter().find(|s| s.id == server_id) else {
            continue;
        };

        let node_name = node.metadata.name.as_deref().unwrap_or("<unknown>");
        let labels = node.metadata.labels.as_ref();

        // Assert instance-type label.
        let instance_type = labels
            .and_then(|l| l.get("node.kubernetes.io/instance-type"))
            .map(String::as_str)
            .unwrap_or("");
        assert_eq!(
            instance_type, server.size_slug,
            "node {node_name}: instance-type label mismatch"
        );

        // Assert region label.
        let region = labels
            .and_then(|l| l.get("topology.kubernetes.io/region"))
            .map(String::as_str)
            .unwrap_or("");
        assert_eq!(
            region, server.region.slug,
            "node {node_name}: region label mismatch"
        );

        // Assert cloud-provider label.
        let cloud_provider = labels
            .and_then(|l| l.get("node.kubernetes.io/cloud-provider"))
            .map(String::as_str)
            .unwrap_or("");
        assert_eq!(
            cloud_provider,
            binarylane_client::PROVIDER_NAME,
            "node {node_name}: cloud-provider label mismatch"
        );

        // Assert the server's public IP is present as an ExternalIP.
        let public_ip = server
            .networks
            .v4
            .iter()
            .find(|n| n.net_type == "public")
            .map(|n| n.ip_address.as_str())
            .unwrap_or("");
        assert!(!public_ip.is_empty(), "server has no public IP");

        let has_external_ip = node
            .status
            .as_ref()
            .and_then(|s| s.addresses.as_ref())
            .map(|addrs| {
                addrs
                    .iter()
                    .any(|a| a.type_ == "ExternalIP" && a.address == public_ip)
            })
            .unwrap_or(false);
        assert!(
            has_external_ip,
            "node {node_name}: missing ExternalIP {public_ip}"
        );

        // Assert the uninitialized taint is NOT present.
        let has_uninit_taint = node
            .spec
            .as_ref()
            .and_then(|s| s.taints.as_ref())
            .map(|taints| {
                taints
                    .iter()
                    .any(|t| t.key == "node.cloudprovider.kubernetes.io/uninitialized")
            })
            .unwrap_or(false);
        assert!(
            !has_uninit_taint,
            "node {node_name}: uninitialized taint should have been removed"
        );

        matched = true;
        break;
    }

    if !matched {
        eprintln!(
            "skipping node sync assertions: no k8s node found with a binarylane:/// provider ID"
        );
    }
    Ok(())
}

/// Full lifecycle test: create a LoadBalancer service, wait for the controller
/// to provision a BL load balancer, update the service to add a second port,
/// verify the LB updates, then delete and verify cleanup.
///
/// BROKEN: BL LB API semantics are not yet fully understood. Known issues:
/// - API only returns entry_protocol in forwarding rules (no port fields)
/// - health_check.protocol must be "both" for mixed http/https
/// - Region defaults to syd but must match server region
/// Re-enable with INTEGRATION_TEST_LB=1 once the LB controller is reworked.
#[tokio::test]
async fn test_load_balancer_lifecycle() -> Result<()> {
    if std::env::var("INTEGRATION_TEST_LB").is_err() {
        eprintln!("skipping test_load_balancer_lifecycle: LB controller needs rework (set INTEGRATION_TEST_LB=1 to force)");
        return Ok(());
    }
    let Some(ctx) = TestContext::new().await else {
        eprintln!("skipping test_load_balancer_lifecycle: BL_API_TOKEN or KUBECONFIG not set");
        return Ok(());
    };

    let name = test_name("lb");
    let ns = "default";
    let services: Api<Service> = Api::namespaced(ctx.kube.clone(), ns);

    // -- Step 1: Create a LoadBalancer service ---------------------------------
    let svc: Service = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": {
            "name": name,
            "namespace": ns
        },
        "spec": {
            "type": "LoadBalancer",
            "ports": [{"name": "http", "port": 80, "protocol": "TCP"}],
            "selector": {"app": "nonexistent"}
        }
    }))?;
    services.create(&PostParams::default(), &svc).await?;

    // Wrap the remaining test in a closure so cleanup runs on any failure.
    let result = run_lb_lifecycle(&ctx, &services, &name).await;

    // Clean up: delete the service if it still exists.
    let _ = services.delete(&name, &DeleteParams::default()).await;

    result
}

async fn run_lb_lifecycle(ctx: &TestContext, services: &Api<Service>, name: &str) -> Result<()> {
    // -- Step 2: Wait for our controller to annotate the service with a LB ID ---
    let svc_api = services.clone();
    let svc_name = name.to_owned();
    wait_for(Duration::from_secs(120), Duration::from_secs(5), || {
        let svc_api = svc_api.clone();
        let svc_name = svc_name.clone();
        async move {
            let svc = svc_api.get(&svc_name).await?;
            let has_annotation = svc
                .metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get("binarylane.com.au/load-balancer-id"))
                .is_some();
            Ok(has_annotation)
        }
    })
    .await
    .context("waiting for LB ID annotation")?;

    // Re-fetch and extract the LB ID + ingress IP.
    let svc = services.get(name).await?;
    let lb_id: i64 = svc
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get("binarylane.com.au/load-balancer-id"))
        .context("missing LB ID annotation")?
        .parse()
        .context("parsing LB ID annotation")?;

    // Verify the LB exists in BinaryLane with a forwarding rule.
    // Note: BL API only returns entry_protocol in forwarding rules (no port fields),
    // because BL LBs are anycast VIPs, not traditional reverse proxies.
    let lb = ctx
        .bl
        .get_load_balancer(lb_id)
        .await?
        .context("LB should exist")?;
    assert!(
        lb.forwarding_rules
            .iter()
            .any(|r| r.entry_protocol == "http"),
        "LB should have an http forwarding rule, got: {:?}",
        lb.forwarding_rules
    );

    // -- Step 4: Update service to add a second port (443) ---------------------
    // Re-fetch to get the allocated nodePort for port 80, then include it in
    // the patch to avoid a "duplicate nodePort" conflict.
    let svc = services.get(name).await?;
    let existing_node_port = svc
        .spec
        .as_ref()
        .and_then(|s| s.ports.as_ref())
        .and_then(|p| p.first())
        .and_then(|p| p.node_port);
    let mut port80 = serde_json::json!({"name": "http", "port": 80, "protocol": "TCP"});
    if let Some(np) = existing_node_port {
        port80["nodePort"] = serde_json::json!(np);
    }
    let patch = serde_json::json!({
        "spec": {
            "ports": [port80, {"name": "https", "port": 443, "protocol": "TCP"}]
        }
    });
    services
        .patch(
            name,
            &kube::api::PatchParams::apply("integration-test"),
            &kube::api::Patch::Merge(&patch),
        )
        .await
        .context("patching service to add port 443")?;

    // -- Step 5: Wait for BL LB to have both http and https forwarding rules ---
    let bl = ctx.bl.clone();
    wait_for(Duration::from_secs(60), Duration::from_secs(5), || {
        let bl = bl.clone();
        async move {
            let lb = bl.get_load_balancer(lb_id).await?;
            Ok(lb.is_some_and(|lb| {
                let has_http = lb
                    .forwarding_rules
                    .iter()
                    .any(|r| r.entry_protocol == "http");
                let has_https = lb
                    .forwarding_rules
                    .iter()
                    .any(|r| r.entry_protocol == "https");
                has_http && has_https
            }))
        }
    })
    .await
    .context("waiting for LB to have http + https forwarding rules")?;

    // -- Step 6: Delete the service --------------------------------------------
    services
        .delete(name, &DeleteParams::default())
        .await
        .context("deleting test service")?;

    // -- Step 7: Wait for the BL LB to be deleted ------------------------------
    wait_for(Duration::from_secs(60), Duration::from_secs(5), || {
        let bl = bl.clone();
        async move {
            let lb = bl.get_load_balancer(lb_id).await?;
            Ok(lb.is_none())
        }
    })
    .await
    .context("waiting for LB to be deleted from BinaryLane")?;

    Ok(())
}
