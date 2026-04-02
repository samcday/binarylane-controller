mod autoscaler;
use binarylane_client as binarylane;
mod controllers;
mod dns_webhook;

pub mod proto {
    tonic::include_proto!("clusterautoscaler.cloudprovider.v1.externalgrpc");
}

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(name = "binarylane-controller")]
struct Args {
    /// BinaryLane API token
    #[arg(long, env = "BL_API_TOKEN")]
    bl_api_token: String,

    /// Path to autoscaler config JSON
    #[arg(
        long,
        env = "CONFIG_PATH",
        default_value = "/etc/binarylane-controller/config.json"
    )]
    config_path: String,

    /// Path to cloud-init template
    #[arg(
        long,
        env = "CLOUD_INIT_PATH",
        default_value = "/etc/binarylane-controller/cloud-init.sh"
    )]
    cloud_init_path: String,

    /// gRPC listen address
    #[arg(long, env = "GRPC_LISTEN_ADDR", default_value = "0.0.0.0:8086")]
    grpc_listen_addr: String,

    /// external-dns webhook listen address
    #[arg(long, env = "EXTERNAL_DNS_LISTEN_ADDR")]
    external_dns_listen_addr: Option<String>,

    /// Enable external-dns webhook server
    #[arg(long, env = "EXTERNAL_DNS_WEBHOOK", default_value_t = false)]
    external_dns_webhook: bool,

    /// Enable cluster-autoscaler gRPC service
    #[arg(long, env = "CLUSTER_AUTOSCALER_SERVICE", default_value_t = false)]
    cluster_autoscaler_service: bool,

    /// Namespace the controller runs in (for namespaced Secret reconciliation)
    #[arg(long, env = "POD_NAMESPACE", default_value = "binarylane-system")]
    pod_namespace: String,

    /// TLS certificate path (enables mTLS when all three TLS args are set)
    #[arg(long, env = "TLS_CERT_PATH")]
    tls_cert_path: Option<String>,

    /// TLS private key path
    #[arg(long, env = "TLS_KEY_PATH")]
    tls_key_path: Option<String>,

    /// TLS CA certificate path
    #[arg(long, env = "TLS_CA_PATH")]
    tls_ca_path: Option<String>,

    /// Comma-separated list of controllers to run.
    /// Use '*' for all defaults, '-name' to exclude.
    #[arg(long, env = "CONTROLLERS", default_value = "*")]
    controllers: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let enabled = controllers::resolve_controllers(&args.controllers);
    let bl = binarylane::Client::new(args.bl_api_token);
    let k8s = kube::Client::try_default()
        .await
        .context("building kubernetes client")?;

    let ctx = std::sync::Arc::new(controllers::ReconcileContext {
        bl: bl.clone(),
        k8s: k8s.clone(),
        secret_namespace: args.pod_namespace.clone(),
    });

    let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    if enabled.contains("node-sync") {
        let ctx = ctx.clone();
        handles.push(tokio::spawn(async move {
            info!(interval = ?Duration::from_secs(30), "node-sync controller starting");
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                controllers::node_sync::reconcile(&ctx).await;
            }
        }));
    }

    if enabled.contains("node-deletion") {
        let ctx = ctx.clone();
        handles.push(tokio::spawn(async move {
            info!(interval = ?Duration::from_secs(30), "node-deletion controller starting");
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                controllers::node_deletion::reconcile(&ctx).await;
            }
        }));
    }

    if enabled.contains("node-bind") {
        let ctx = ctx.clone();
        handles.push(tokio::spawn(async move {
            info!(interval = ?Duration::from_secs(30), "node-bind controller starting");
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                controllers::node_bind::reconcile(&ctx).await;
            }
        }));
    }

    if enabled.contains("node-provision") {
        let ctx = ctx.clone();
        handles.push(tokio::spawn(async move {
            info!(interval = ?Duration::from_secs(30), "node-provision controller starting");
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                controllers::node_provision::reconcile(&ctx).await;
            }
        }));
    }

    if enabled.contains("service") {
        let ctx = ctx.clone();
        handles.push(tokio::spawn(async move {
            info!(interval = ?Duration::from_secs(30), "service controller starting");
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                controllers::service::reconcile(&ctx).await;
            }
        }));
    }

    // Monitor controller tasks - exit if any die
    if !handles.is_empty() {
        tokio::spawn(async move {
            let (result, _index, _remaining) = futures::future::select_all(handles).await;
            match result {
                Ok(_) => error!("controller exited unexpectedly"),
                Err(e) => error!(error = %e, "controller panicked"),
            }
            std::process::exit(1);
        });
    }

    let tls_vars = [&args.tls_cert_path, &args.tls_key_path, &args.tls_ca_path];
    let tls_set = tls_vars.iter().filter(|v| v.is_some()).count();
    if tls_set > 0 && tls_set < 3 {
        warn!(
            "partial TLS config: all three of TLS_CERT_PATH, TLS_KEY_PATH, TLS_CA_PATH must be set to enable mTLS, falling back to plaintext"
        );
    }

    let shared_tls = if let (Some(cert_path), Some(key_path), Some(ca_path)) =
        (&args.tls_cert_path, &args.tls_key_path, &args.tls_ca_path)
    {
        let cert_pem = tokio::fs::read(cert_path)
            .await
            .context("reading TLS cert")?;
        let key_pem = tokio::fs::read(key_path).await.context("reading TLS key")?;
        let ca_pem = tokio::fs::read(ca_path)
            .await
            .context("reading TLS CA cert")?;

        Some(dns_webhook::TlsConfig {
            cert_pem,
            key_pem,
            ca_pem,
        })
    } else {
        None
    };

    if let Some(listen_addr) = args.external_dns_listen_addr.clone()
        && args.external_dns_webhook
    {
        let addr = listen_addr
            .parse()
            .context("parsing external-dns listen address")?;
        let bl_dns = bl.clone();
        let tls = shared_tls.clone();
        tokio::spawn(async move {
            info!(addr = %listen_addr, "external-dns webhook starting");
            if let Err(e) = dns_webhook::run(bl_dns, addr, tls).await {
                error!(error = %e, "external-dns webhook server error");
                std::process::exit(1);
            }
        });
    }

    // Load autoscaler config
    if !args.cluster_autoscaler_service {
        std::future::pending::<()>().await;
        return Ok(());
    }

    let cfg_data = match tokio::fs::read_to_string(&args.config_path).await {
        Ok(data) => data,
        Err(e) => {
            warn!(path = %args.config_path, error = %e, "autoscaler config not found, gRPC provider disabled");
            std::future::pending::<()>().await;
            return Ok(());
        }
    };

    let mut cfg: autoscaler::Config =
        serde_json::from_str(&cfg_data).context("parsing autoscaler config")?;

    let cloud_init = tokio::fs::read_to_string(&args.cloud_init_path)
        .await
        .context("reading cloud-init template")?;
    cfg.cloud_init = cloud_init;

    cfg.template_vars = std::env::vars()
        .filter_map(|(k, v)| k.strip_prefix("TMPL_").map(|k| (k.to_string(), v)))
        .collect::<HashMap<_, _>>();

    let provider = autoscaler::Provider::new(k8s.clone(), cfg.clone(), args.pod_namespace.clone());
    let svc = proto::cloud_provider_server::CloudProviderServer::new(provider);

    info!(
        grpc = %args.grpc_listen_addr,
        node_groups = cfg.node_groups.len(),
        "binarylane-controller starting"
    );

    let addr = args
        .grpc_listen_addr
        .parse()
        .context("parsing listen address")?;

    let mut server = Server::builder();

    if let Some(tls) = &shared_tls {
        let tls_config = ServerTlsConfig::new()
            .identity(Identity::from_pem(&tls.cert_pem, &tls.key_pem))
            .client_ca_root(Certificate::from_pem(&tls.ca_pem));

        server = server.tls_config(tls_config).context("configuring mTLS")?;
        info!("mTLS enabled for gRPC server");
    }

    server
        .add_service(svc)
        .serve(addr)
        .await
        .context("gRPC server error")?;

    Ok(())
}
