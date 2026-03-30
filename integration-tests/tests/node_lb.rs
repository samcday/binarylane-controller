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

    assert!(
        matched,
        "no k8s node found with a binarylane:/// provider ID matching a BL server"
    );
    Ok(())
}

/// Full lifecycle test: create a LoadBalancer service, wait for the controller
/// to provision a BL load balancer, update the service to add a second port,
/// verify the LB updates, then delete and verify cleanup.
#[tokio::test]
async fn test_load_balancer_lifecycle() -> Result<()> {
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
            "ports": [{"port": 80, "protocol": "TCP"}],
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
    // -- Step 2: Wait for an ingress IP ----------------------------------------
    let svc_api = services.clone();
    let svc_name = name.to_owned();
    wait_for(Duration::from_secs(120), Duration::from_secs(5), || {
        let svc_api = svc_api.clone();
        let svc_name = svc_name.clone();
        async move {
            let svc = svc_api.get(&svc_name).await?;
            let has_ip = svc
                .status
                .as_ref()
                .and_then(|s| s.load_balancer.as_ref())
                .and_then(|lb| lb.ingress.as_ref())
                .and_then(|ing| ing.first())
                .and_then(|i| i.ip.as_deref())
                .is_some_and(|ip| !ip.is_empty());
            Ok(has_ip)
        }
    })
    .await
    .context("waiting for ingress IP")?;

    // Re-fetch and extract the ingress IP.
    let svc = services.get(name).await?;
    let ingress_ip = svc
        .status
        .as_ref()
        .and_then(|s| s.load_balancer.as_ref())
        .and_then(|lb| lb.ingress.as_ref())
        .and_then(|ing| ing.first())
        .and_then(|i| i.ip.clone())
        .unwrap_or_default();
    assert!(!ingress_ip.is_empty(), "ingress IP should not be empty");

    // -- Step 3: Extract the LB ID from annotation -----------------------------
    let lb_id: i64 = svc
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get("binarylane.com.au/load-balancer-id"))
        .context("missing LB ID annotation")?
        .parse()
        .context("parsing LB ID annotation")?;

    // Verify the LB exists in BinaryLane with the correct forwarding rule.
    let lb = ctx
        .bl
        .get_load_balancer(lb_id)
        .await?
        .context("LB should exist")?;
    assert!(
        lb.forwarding_rules
            .iter()
            .any(|r| r.entry_port == 80 && r.entry_protocol == "tcp"),
        "LB should have a forwarding rule for port 80"
    );

    // -- Step 4: Update service to add a second port (443) ---------------------
    let patch = serde_json::json!({
        "spec": {
            "ports": [
                {"port": 80, "protocol": "TCP"},
                {"port": 443, "protocol": "TCP"}
            ]
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

    // -- Step 5: Wait for BL LB to have 2 forwarding rules --------------------
    let bl = ctx.bl.clone();
    wait_for(Duration::from_secs(60), Duration::from_secs(5), || {
        let bl = bl.clone();
        async move {
            let lb = bl.get_load_balancer(lb_id).await?;
            Ok(lb.is_some_and(|lb| lb.forwarding_rules.len() == 2))
        }
    })
    .await
    .context("waiting for LB to have 2 forwarding rules")?;

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
