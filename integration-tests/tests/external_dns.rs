use std::time::Duration;

use anyhow::Result;
use binarylane_client::DomainRecord;
use integration_tests::{TestContext, test_name, wait_for};
use kube::Api;
use kube::api::{DeleteParams, PostParams};

/// Build the FQDN for a domain record (handles bare "@" and empty names).
fn record_fqdn(record: &DomainRecord, domain: &str) -> String {
    if record.name.is_empty() || record.name == "@" {
        domain.to_string()
    } else {
        format!("{}.{domain}", record.name)
    }
}

/// Test the external-dns → webhook → BL DNS API integration using
/// DNSEndpoint CRDs (avoids dependency on working LoadBalancer controller).
#[tokio::test]
async fn test_external_dns_record_lifecycle() -> Result<()> {
    let ctx = match TestContext::new().await {
        Some(ctx) => ctx,
        None => {
            eprintln!("skipping: BL_API_TOKEN or KUBECONFIG not set");
            return Ok(());
        }
    };

    // Check that external-dns is deployed in the cluster.
    let deploys: kube::Api<k8s_openapi::api::apps::v1::Deployment> =
        kube::Api::namespaced(ctx.kube.clone(), "binarylane-system");
    match deploys.get("external-dns").await {
        Ok(_) => {}
        Err(kube::Error::Api(e)) if e.code == 404 => {
            eprintln!(
                "skipping: external-dns deployment not found (deploy full stack via Tilt first)"
            );
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    }

    // List BL domains and skip if none are configured.
    let domains = ctx.bl.list_domains().await?;
    if domains.is_empty() {
        eprintln!("skipping: no BL domains configured");
        return Ok(());
    }
    let domain = &domains[0].name;
    eprintln!("using domain: {domain}");

    // Generate a unique subdomain.
    let subdomain = test_name("dns");
    let hostname = format!("{subdomain}.{domain}");
    let target_ip = "203.0.113.42"; // TEST-NET-3 (RFC 5737), won't conflict
    eprintln!("test hostname: {hostname} -> {target_ip}");

    // Create a DNSEndpoint CRD resource. This is consumed by external-dns's
    // CRD source, which then pushes the record through our webhook provider.
    let endpoints: Api<kube::api::DynamicObject> = Api::namespaced_with(
        ctx.kube.clone(),
        "default",
        &kube::discovery::ApiResource {
            group: "externaldns.k8s.io".into(),
            version: "v1alpha1".into(),
            api_version: "externaldns.k8s.io/v1alpha1".into(),
            kind: "DNSEndpoint".into(),
            plural: "dnsendpoints".into(),
        },
    );

    let endpoint: kube::api::DynamicObject = serde_json::from_value(serde_json::json!({
        "apiVersion": "externaldns.k8s.io/v1alpha1",
        "kind": "DNSEndpoint",
        "metadata": {
            "name": subdomain,
            "namespace": "default"
        },
        "spec": {
            "endpoints": [{
                "dnsName": hostname,
                "recordTTL": 300,
                "recordType": "A",
                "targets": [target_ip]
            }]
        }
    }))?;

    endpoints.create(&PostParams::default(), &endpoint).await?;
    eprintln!("created DNSEndpoint: {subdomain}");

    // Cleanup helper: delete DNSEndpoint and leftover DNS records.
    let cleanup = |ctx: &TestContext, name: &str, domain: &str, hostname: &str| {
        let bl = ctx.bl.clone();
        let kube = ctx.kube.clone();
        let name = name.to_string();
        let domain = domain.to_string();
        let hostname = hostname.to_string();
        async move {
            let endpoints: Api<kube::api::DynamicObject> = Api::namespaced_with(
                kube,
                "default",
                &kube::discovery::ApiResource {
                    group: "externaldns.k8s.io".into(),
                    version: "v1alpha1".into(),
                    api_version: "externaldns.k8s.io/v1alpha1".into(),
                    kind: "DNSEndpoint".into(),
                    plural: "dnsendpoints".into(),
                },
            );
            let _ = endpoints.delete(&name, &DeleteParams::default()).await;

            if let Ok(records) = bl.list_domain_records(&domain).await {
                for record in records {
                    let fqdn = record_fqdn(&record, &domain);
                    let is_ours = fqdn == hostname
                        && (record.record_type == "A"
                            || (record.record_type == "TXT"
                                && record.data.contains("heritage=external-dns")));
                    if is_ours {
                        eprintln!(
                            "cleanup: deleting record {} ({} {})",
                            record.id, record.record_type, record.name
                        );
                        let _ = bl.delete_domain_record(&domain, record.id).await;
                    }
                }
            }
        }
    };

    // Wait for the A record to appear in the BL DNS API.
    let bl_wait = ctx.bl.clone();
    let domain_wait = domain.clone();
    let hostname_wait = hostname.clone();
    let wait_result = wait_for(Duration::from_secs(120), Duration::from_secs(5), || {
        let bl = bl_wait.clone();
        let domain = domain_wait.clone();
        let hostname = hostname_wait.clone();
        async move {
            let records = bl.list_domain_records(&domain).await?;
            let has_a = records
                .iter()
                .any(|r| r.record_type == "A" && record_fqdn(r, &domain) == hostname);
            Ok(has_a)
        }
    })
    .await;

    if let Err(e) = &wait_result {
        eprintln!("A record wait failed: {e}");
        cleanup(&ctx, &subdomain, domain, &hostname).await;
        return Err(wait_result.unwrap_err());
    }
    eprintln!("A record appeared for {hostname}");

    // Verify the A record points to the correct IP.
    let records = ctx.bl.list_domain_records(domain).await?;
    let a_record = records
        .iter()
        .find(|r| r.record_type == "A" && record_fqdn(r, domain) == hostname);
    if let Some(a) = a_record {
        assert_eq!(
            a.data, target_ip,
            "A record should point to {target_ip}, got {}",
            a.data
        );
        eprintln!("A record verified: {} -> {}", hostname, a.data);
    }

    // Check for TXT ownership record.
    let has_txt = records.iter().any(|r| {
        r.record_type == "TXT"
            && record_fqdn(r, domain) == hostname
            && r.data.contains("heritage=external-dns")
    });
    if has_txt {
        eprintln!("TXT ownership record found for {hostname}");
    }

    // Delete the DNSEndpoint.
    endpoints
        .delete(&subdomain, &DeleteParams::default())
        .await?;
    eprintln!("deleted DNSEndpoint: {subdomain}");

    // Wait for DNS records to be cleaned up.
    let bl_del = ctx.bl.clone();
    let domain_del = domain.clone();
    let hostname_del = hostname.clone();
    let del_result = wait_for(Duration::from_secs(120), Duration::from_secs(5), || {
        let bl = bl_del.clone();
        let domain = domain_del.clone();
        let hostname = hostname_del.clone();
        async move {
            let records = bl.list_domain_records(&domain).await?;
            let still_exists = records.iter().any(|r| {
                record_fqdn(r, &domain) == hostname
                    && (r.record_type == "A" || r.record_type == "TXT")
            });
            Ok(!still_exists)
        }
    })
    .await;

    if let Err(e) = &del_result {
        eprintln!("DNS cleanup wait failed: {e}");
        cleanup(&ctx, &subdomain, domain, &hostname).await;
        return Err(del_result.unwrap_err());
    }

    eprintln!("DNS records cleaned up for {hostname}");
    eprintln!("external-dns record lifecycle test passed");
    Ok(())
}
