mod autoscaler;
use binarylane_client as binarylane;
use binarylane_controller::crd;
mod controllers;
mod dns_webhook;

pub mod proto {
    tonic::include_proto!("clusterautoscaler.cloudprovider.v1.externalgrpc");
}

use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use k8s_openapi::api::core::v1::{Node, Secret, Service};
use kube::Api;
use kube::runtime::{Controller, watcher};
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(name = "binarylane-controller")]
struct Args {
    /// BinaryLane API token
    #[arg(long, env = "BL_API_TOKEN")]
    bl_api_token: String,

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

    /// Namespace in the target cluster where per-node Secrets (password, user-data)
    /// are created/read. Usually matches the controller's own pod namespace, but must
    /// be set explicitly when the controller runs outside the cluster it manages
    /// (e.g. via `--kubeconfig` pointing at a remote control plane).
    #[arg(long, env = "NAMESPACE", default_value = "binarylane-system")]
    namespace: String,

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

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = sigterm.recv() => {}
                }
            }
            Err(e) => {
                warn!(error = %e, "failed to install SIGTERM handler; waiting for SIGINT only");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
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
        secret_namespace: args.namespace.clone(),
        bl_catalog: tokio::sync::RwLock::new(None),
    });

    let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    if enabled.contains("node-sync") {
        let ctx = ctx.clone();
        handles.push(tokio::spawn(async move {
            info!("node-sync controller starting");
            let nodes: Api<Node> = Api::all(ctx.k8s.clone());
            Controller::new(nodes, watcher::Config::default())
                .shutdown_on_signal()
                .run(
                    controllers::node_sync::reconcile,
                    controllers::error_policy,
                    ctx,
                )
                .for_each(|res| async move {
                    match res {
                        Ok((obj, _)) => tracing::trace!(name = %obj.name, "node-sync: reconciled"),
                        Err(e) => tracing::warn!(error = %e, "node-sync: reconcile failed"),
                    }
                })
                .await;
        }));
    }

    if enabled.contains("node-deletion") {
        let ctx = ctx.clone();
        handles.push(tokio::spawn(async move {
            info!("node-deletion controller starting");
            let nodes: Api<Node> = Api::all(ctx.k8s.clone());
            let secrets: Api<Secret> = Api::namespaced(ctx.k8s.clone(), &ctx.secret_namespace);
            Controller::new(nodes, watcher::Config::default())
                .watches(
                    secrets,
                    watcher::Config::default(),
                    controllers::secret_to_node_mapper,
                )
                .shutdown_on_signal()
                .run(
                    controllers::node_deletion::reconcile,
                    controllers::error_policy,
                    ctx,
                )
                .for_each(|res| async move {
                    match res {
                        Ok((obj, _)) => {
                            tracing::trace!(name = %obj.name, "node-deletion: reconciled")
                        }
                        Err(e) => tracing::warn!(error = %e, "node-deletion: reconcile failed"),
                    }
                })
                .await;
        }));
    }

    if enabled.contains("node-bind") {
        let ctx = ctx.clone();
        handles.push(tokio::spawn(async move {
            info!("node-bind controller starting");
            let nodes: Api<Node> = Api::all(ctx.k8s.clone());
            Controller::new(nodes, watcher::Config::default())
                .shutdown_on_signal()
                .run(
                    controllers::node_bind::reconcile,
                    controllers::error_policy,
                    ctx,
                )
                .for_each(|res| async move {
                    match res {
                        Ok((obj, _)) => tracing::trace!(name = %obj.name, "node-bind: reconciled"),
                        Err(e) => tracing::warn!(error = %e, "node-bind: reconcile failed"),
                    }
                })
                .await;
        }));
    }

    if enabled.contains("node-provision") {
        let ctx = ctx.clone();
        handles.push(tokio::spawn(async move {
            info!("node-provision controller starting");
            let nodes: Api<Node> = Api::all(ctx.k8s.clone());
            let secrets: Api<Secret> = Api::namespaced(ctx.k8s.clone(), &ctx.secret_namespace);
            Controller::new(nodes, watcher::Config::default())
                .watches(
                    secrets,
                    watcher::Config::default(),
                    controllers::secret_to_node_mapper,
                )
                .shutdown_on_signal()
                .run(
                    controllers::node_provision::reconcile,
                    controllers::error_policy,
                    ctx,
                )
                .for_each(|res| async move {
                    match res {
                        Ok((obj, _)) => {
                            tracing::trace!(name = %obj.name, "node-provision: reconciled")
                        }
                        Err(e) => tracing::warn!(error = %e, "node-provision: reconcile failed"),
                    }
                })
                .await;
        }));
    }

    if enabled.contains("service") {
        let ctx = ctx.clone();
        handles.push(tokio::spawn(async move {
            info!("service controller starting");
            let services: Api<Service> = Api::all(ctx.k8s.clone());
            Controller::new(services, watcher::Config::default())
                .shutdown_on_signal()
                .run(
                    controllers::service::reconcile,
                    controllers::error_policy,
                    ctx,
                )
                .for_each(|res| async move {
                    match res {
                        Ok((obj, _)) => tracing::trace!(name = %obj.name, "service: reconciled"),
                        Err(e) => tracing::warn!(error = %e, "service: reconcile failed"),
                    }
                })
                .await;
        }));
    }

    // Monitor controller tasks — exit if any panic. Clean exits (e.g. from
    // shutdown_on_signal) are expected and ignored.
    if !handles.is_empty() {
        info!(count = handles.len(), "controllers started");
        tokio::spawn(async move {
            let (result, _index, _remaining) = futures::future::select_all(handles).await;
            match result {
                Ok(_) => {}
                Err(e) => {
                    error!(error = %e, "controller panicked");
                    std::process::exit(1);
                }
            }
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

    if !args.cluster_autoscaler_service {
        shutdown_signal().await;
        info!("shutdown signal received");
        return Ok(());
    }

    let asg_api: kube::Api<crd::AutoScalingGroup> = kube::Api::all(k8s.clone());
    let (asg_store, asg_writer) = kube::runtime::reflector::store();
    tokio::spawn(async move {
        kube::runtime::reflector::reflector(
            asg_writer,
            kube::runtime::watcher::watcher(asg_api, kube::runtime::watcher::Config::default()),
        )
        .for_each(|event| async {
            if let Err(e) = event {
                warn!(error = %e, "autoscaler ASG reflector watch error");
            }
        })
        .await;
    });

    let provider =
        autoscaler::Provider::new(k8s.clone(), bl.clone(), asg_store, args.namespace.clone());
    let svc = proto::cloud_provider_server::CloudProviderServer::new(provider);

    info!(
        grpc = %args.grpc_listen_addr,
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
        .serve_with_shutdown(addr, async {
            shutdown_signal().await;
            info!("shutdown signal received");
        })
        .await
        .context("gRPC server error")?;

    Ok(())
}
