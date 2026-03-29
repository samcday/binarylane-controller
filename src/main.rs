mod autoscaler;
mod binarylane;
mod node_controller;
mod service_controller;

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

    /// TLS certificate path (enables mTLS when all three TLS args are set)
    #[arg(long, env = "TLS_CERT_PATH")]
    tls_cert_path: Option<String>,

    /// TLS private key path
    #[arg(long, env = "TLS_KEY_PATH")]
    tls_key_path: Option<String>,

    /// TLS CA certificate path
    #[arg(long, env = "TLS_CA_PATH")]
    tls_ca_path: Option<String>,
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
    let bl = binarylane::Client::new(args.bl_api_token);
    let k8s = kube::Client::try_default()
        .await
        .context("building kubernetes client")?;

    // Start node controller
    let bl_nc = bl.clone();
    let k8s_nc = k8s.clone();
    let node_handle = tokio::spawn(async move {
        info!(interval = ?Duration::from_secs(30), "node controller starting");
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            node_controller::reconcile(&bl_nc, &k8s_nc).await;
        }
    });

    // Start service controller
    let bl_sc = bl.clone();
    let k8s_sc = k8s.clone();
    let svc_handle = tokio::spawn(async move {
        info!(interval = ?Duration::from_secs(30), "service controller starting");
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            service_controller::reconcile(&bl_sc, &k8s_sc).await;
        }
    });

    // Monitor controller tasks - exit if either dies
    tokio::spawn(async move {
        tokio::select! {
            result = node_handle => {
                match result {
                    Ok(_) => error!("node controller exited unexpectedly"),
                    Err(e) => error!(error = %e, "node controller panicked"),
                }
            }
            result = svc_handle => {
                match result {
                    Ok(_) => error!("service controller exited unexpectedly"),
                    Err(e) => error!(error = %e, "service controller panicked"),
                }
            }
        }
        std::process::exit(1);
    });

    // Load autoscaler config
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

    let provider = autoscaler::Provider::new(bl, k8s.clone(), cfg.clone());
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

    let tls_vars = [&args.tls_cert_path, &args.tls_key_path, &args.tls_ca_path];
    let tls_set = tls_vars.iter().filter(|v| v.is_some()).count();
    if tls_set > 0 && tls_set < 3 {
        warn!(
            "partial TLS config: all three of TLS_CERT_PATH, TLS_KEY_PATH, TLS_CA_PATH must be set to enable mTLS, falling back to plaintext"
        );
    }

    if let (Some(cert_path), Some(key_path), Some(ca_path)) =
        (&args.tls_cert_path, &args.tls_key_path, &args.tls_ca_path)
    {
        let cert = tokio::fs::read(cert_path)
            .await
            .context("reading TLS cert")?;
        let key = tokio::fs::read(key_path).await.context("reading TLS key")?;
        let ca_cert = tokio::fs::read(ca_path)
            .await
            .context("reading TLS CA cert")?;

        let tls_config = ServerTlsConfig::new()
            .identity(Identity::from_pem(&cert, &key))
            .client_ca_root(Certificate::from_pem(&ca_cert));

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
