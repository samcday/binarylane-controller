use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use base64::Engine;
use bcrypt::hash;
use clap::{Args, Parser, Subcommand};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{Namespace, Node, Secret, Service, ServiceAccount};
use kube::api::{Patch, PatchParams};
use kube::{Api, ResourceExt};
use rand::distributions::Alphanumeric;
use rand::{Rng, thread_rng};
use serde::{Deserialize, Serialize};

const DEFAULT_API_BASE: &str = "https://api.binarylane.com.au/v2";

const DEFAULT_STATE_FILE: &str = ".dev/dev-state.json";
const DEFAULT_KUBECONFIG_OUT: &str = ".dev/kubeconfig";
const DEFAULT_KNOWN_HOSTS: &str = ".dev/known_hosts";
const DEFAULT_DOCKER_CONFIG_DIR: &str = ".dev/docker";
const DEFAULT_TILT_VALUES_OUT: &str = ".dev/tilt-values.generated.yaml";
const DEFAULT_REGISTRY_USERNAME: &str = "dev";
const DEFAULT_REGISTRY_SECRET_NAME: &str = "dev-registry-cred";
const REGISTRY_NAMESPACE: &str = "binarylane-dev-registry";
const REGISTRY_DATA_HOSTPATH: &str = "/var/lib/binarylane-dev-registry/registry-data";
const DEV_AUTOSCALER_GROUP_ID: &str = "workers";

const DEFAULT_REGION: &str = "bne";
const REGION_FALLBACKS: &[&str] = &["syd", "mel", "adl", "per", "sin"];
const DEFAULT_SIZE: &str = "std-1vcpu";
const DEFAULT_IMAGE: &str = "ubuntu-24.04";
const DEFAULT_SSH_USER: &str = "root";
const DEFAULT_MANAGED_SSH_KEY_PATH: &str = ".dev/dev-control-plane-ed25519";

#[derive(Parser)]
#[command(name = "xtask", about = "Development tasks for binarylane-controller")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create/reuse a remote BinaryLane dev control plane and emit sourceable env vars
    DevUp(Box<DevUpArgs>),
    /// Tear down the remote BinaryLane dev control plane from local state
    DevDown(DevDownArgs),
    /// Launch Tilt with project-appropriate defaults (--port 0 to avoid collisions)
    Tilt(TiltArgs),
}

#[derive(Debug, Args)]
struct DevUpArgs {
    /// BinaryLane API token
    #[arg(long, env = "BL_API_TOKEN")]
    bl_api_token: String,

    /// State file path used for idempotency
    #[arg(long, default_value = DEFAULT_STATE_FILE)]
    state_file: PathBuf,

    /// Output kubeconfig path
    #[arg(long, default_value = DEFAULT_KUBECONFIG_OUT)]
    kubeconfig_out: PathBuf,

    /// known_hosts file used by SSH (kept local to this repo)
    #[arg(long, default_value = DEFAULT_KNOWN_HOSTS)]
    known_hosts: PathBuf,

    /// Local Docker config directory containing registry auth
    #[arg(long, default_value = DEFAULT_DOCKER_CONFIG_DIR)]
    docker_config_dir: PathBuf,

    /// Generated tilt values file with autoscaler + mTLS dev config
    #[arg(long, default_value = DEFAULT_TILT_VALUES_OUT)]
    tilt_values_out: PathBuf,

    /// BinaryLane region slug
    #[arg(long, env = "BL_DEV_REGION", default_value = DEFAULT_REGION)]
    region: String,

    /// BinaryLane size slug
    #[arg(long, env = "BL_DEV_SIZE", default_value = DEFAULT_SIZE)]
    size: String,

    /// BinaryLane image slug
    #[arg(long, env = "BL_DEV_IMAGE", default_value = DEFAULT_IMAGE)]
    image: String,

    /// SSH user for the control-plane node
    #[arg(long, env = "BL_DEV_SSH_USER", default_value = DEFAULT_SSH_USER)]
    ssh_user: String,

    /// Username for the dev registry auth
    #[arg(long, env = "BL_DEV_REGISTRY_USERNAME", default_value = DEFAULT_REGISTRY_USERNAME)]
    registry_username: String,

    /// Kubernetes Secret name used for image pull auth
    #[arg(
        long,
        env = "BL_DEV_REGISTRY_SECRET_NAME",
        default_value = DEFAULT_REGISTRY_SECRET_NAME
    )]
    registry_secret_name: String,

    /// Timeout used for server/SSH/k3s readiness
    #[arg(long, env = "BL_DEV_WAIT_TIMEOUT_SECS", default_value_t = 900)]
    wait_timeout_secs: u64,
}

#[derive(Debug, Args)]
struct TiltArgs {
    /// Extra arguments passed through to `tilt`
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

#[derive(Debug, Args)]
struct DevDownArgs {
    /// BinaryLane API token (required only if a tracked server must be deleted)
    #[arg(long, env = "BL_API_TOKEN")]
    bl_api_token: Option<String>,

    /// State file path used for idempotency
    #[arg(long, default_value = DEFAULT_STATE_FILE)]
    state_file: PathBuf,

    /// Output kubeconfig path to delete
    #[arg(long, default_value = DEFAULT_KUBECONFIG_OUT)]
    kubeconfig_out: PathBuf,

    /// known_hosts path to delete
    #[arg(long, default_value = DEFAULT_KNOWN_HOSTS)]
    known_hosts: PathBuf,

    /// Local Docker config directory to delete
    #[arg(long, default_value = DEFAULT_DOCKER_CONFIG_DIR)]
    docker_config_dir: PathBuf,

    /// Generated tilt values file to delete
    #[arg(long, default_value = DEFAULT_TILT_VALUES_OUT)]
    tilt_values_out: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct DevState {
    server_id: Option<i64>,
    server_name: String,
    server_ip: Option<String>,
    region: String,
    size: String,
    image: String,
    ssh_user: String,
    ssh_key_path: Option<String>,
    ssh_key_id: Option<i64>,
    ssh_key_fingerprint: Option<String>,
    ssh_key_name: String,
    registry_host: Option<String>,
    registry_username: String,
    registry_password: Option<String>,
    registry_htpasswd: Option<String>,
    registry_pull_secret_name: String,
    docker_config_dir: Option<String>,
    tilt_values_path: Option<String>,
    registry_image_repo: Option<String>,
    kubeconfig_path: Option<String>,
    known_hosts_path: Option<String>,
    k3s_url: Option<String>,
}

#[derive(Clone)]
struct BlClient {
    token: String,
    api_base: String,
    http: reqwest::blocking::Client,
}

#[derive(Debug, Clone, Deserialize)]
struct Server {
    id: i64,
    name: String,
    status: String,
    region: ServerRegion,
    networks: Networks,
}

#[derive(Debug, Clone, Deserialize)]
struct ServerRegion {
    slug: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Networks {
    v4: Vec<NetworkV4>,
}

#[derive(Debug, Clone, Deserialize)]
struct NetworkV4 {
    ip_address: String,
    #[serde(rename = "type")]
    net_type: String,
}

#[derive(Debug, Clone, Deserialize)]
struct LoadBalancer {
    id: i64,
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct AccountSshKey {
    id: i64,
    fingerprint: String,
    public_key: String,
    name: String,
}

#[derive(Debug, Serialize)]
struct CreateServerRequest {
    name: String,
    size: String,
    image: String,
    region: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_data: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ssh_keys: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    password: Option<String>,
}

#[derive(Debug, Serialize)]
struct CreateAccountSshKeyRequest {
    name: String,
    public_key: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::DevUp(args) => cmd_dev_up(*args),
        Commands::DevDown(args) => cmd_dev_down(args),
        Commands::Tilt(args) => cmd_tilt(args),
    }
}

fn cmd_dev_up(args: DevUpArgs) -> Result<()> {
    let t_total = Instant::now();

    ensure_tool("ssh", "install OpenSSH client")?;

    let cluster_name = get_or_create_cluster_name()?;
    let server_name = format!("{cluster_name}-controlplane");
    let ssh_key_name = format!("{cluster_name}-key");

    ensure_tool("ssh-keygen", "install OpenSSH tools")?;

    ensure_parent_dir(&args.state_file)?;
    ensure_parent_dir(&args.kubeconfig_out)?;
    ensure_parent_dir(&args.known_hosts)?;
    let ssh_key_path_buf = PathBuf::from(DEFAULT_MANAGED_SSH_KEY_PATH);
    ensure_parent_dir(&ssh_key_path_buf)?;
    ensure_parent_dir(&args.tilt_values_out)?;

    let timeout = Duration::from_secs(args.wait_timeout_secs);
    let ssh_key_path = absolute_path(&ssh_key_path_buf)?;
    let docker_config_dir = absolute_path(&args.docker_config_dir)?;
    let tilt_values_out = absolute_path(&args.tilt_values_out)?;
    let dev_resources_out = tilt_values_out
        .parent()
        .unwrap_or(Path::new("."))
        .join("dev-resources.generated.yaml");
    ensure_dir(&docker_config_dir)?;
    ensure_managed_ssh_keypair(&ssh_key_path)?;
    let ssh_public_key = read_managed_public_key(&ssh_key_path)?;

    let mut state = load_state(&args.state_file)?;
    if state.server_name.is_empty() {
        state.server_name = server_name.to_string();
    }

    let client = BlClient::new(args.bl_api_token.clone())?;
    let account_key = ensure_account_ssh_key(
        &client,
        &ssh_key_name,
        &ssh_public_key,
        state.ssh_key_id,
        state.ssh_key_fingerprint.as_deref(),
    )?;

    let mut server = if let Some(server_id) = state.server_id {
        match client.get_server(server_id)? {
            Some(existing) => {
                eprintln!(
                    "Reusing server from state: {} (id={})",
                    existing.name, existing.id
                );
                existing
            }
            None => {
                eprintln!(
                    "State referenced server id={} but it no longer exists; recreating",
                    server_id
                );
                create_dev_server(
                    &client,
                    &args,
                    &server_name,
                    &ssh_key_name,
                    &account_key.fingerprint,
                )?
            }
        }
    } else {
        create_dev_server(
            &client,
            &args,
            &server_name,
            &ssh_key_name,
            &account_key.fingerprint,
        )?
    };

    state.server_id = Some(server.id);
    state.server_name = server.name.clone();
    state.region = server.region.slug.clone();
    state.size = args.size.clone();
    state.image = args.image.clone();
    state.ssh_key_path = Some(path_to_string(&ssh_key_path)?);
    state.ssh_key_id = Some(account_key.id);
    state.ssh_key_fingerprint = Some(account_key.fingerprint.clone());
    state.ssh_key_name = account_key.name.clone();
    state.tilt_values_path = Some(path_to_string(&tilt_values_out)?);
    save_state(&args.state_file, &state)?;

    let t = Instant::now();
    server = wait_for_server_active(&client, server.id, timeout)?;
    eprintln!(
        "  server active ........... {:.1}s",
        t.elapsed().as_secs_f64()
    );
    let server_ip = server
        .public_ipv4()
        .ok_or_else(|| anyhow::anyhow!("server {} has no public IPv4", server.id))?;
    let registry_host = format!("{}:30500", server_ip);

    // Keep the previously persisted username when present so repeated runs remain idempotent.
    let registry_username = if state.registry_username.trim().is_empty() {
        args.registry_username.clone()
    } else {
        state.registry_username.clone()
    };
    let registry_password = state
        .registry_password
        .clone()
        .unwrap_or_else(generate_registry_password);
    // Reuse persisted htpasswd when password hasn't changed, since bcrypt salts differ each run.
    let registry_htpasswd = match &state.registry_htpasswd {
        Some(h) if state.registry_password.as_deref() == Some(&registry_password) => h.clone(),
        _ => {
            // Cost 4 (minimum) is fine for a dev-only registry credential.
            let bcrypt =
                hash(&registry_password, 4).context("hashing registry password for basic auth")?;
            format!("{}:{}\n", registry_username, bcrypt)
        }
    };

    state.registry_host = Some(registry_host.clone());
    state.registry_username = registry_username.clone();
    state.registry_password = Some(registry_password.clone());
    state.registry_htpasswd = Some(registry_htpasswd.clone());
    state.registry_pull_secret_name = args.registry_secret_name.clone();
    state.docker_config_dir = Some(path_to_string(&docker_config_dir)?);
    state.registry_image_repo = Some(format!("{}/binarylane-controller", registry_host));
    save_state(&args.state_file, &state)?;

    let mut ssh_users = vec![args.ssh_user.clone()];
    for fallback in ["root", "ubuntu", "debian"] {
        if !ssh_users.iter().any(|u| u == fallback) {
            ssh_users.push(fallback.to_string());
        }
    }

    let t = Instant::now();
    let ssh_user = wait_for_ssh_ready(
        &server_ip,
        &ssh_users,
        Some(&ssh_key_path),
        &args.known_hosts,
        timeout,
    )?;
    eprintln!(
        "  ssh ready ............... {:.1}s",
        t.elapsed().as_secs_f64()
    );

    let k3s_registry = K3sRegistryConfig {
        host: &registry_host,
        username: &registry_username,
        password: &registry_password,
    };

    let t = Instant::now();
    ensure_k3s_server(
        &server_ip,
        &ssh_user,
        Some(&ssh_key_path),
        &args.known_hosts,
        &k3s_registry,
        timeout,
        &server_name,
    )?;
    eprintln!(
        "  k3s ready ............... {:.1}s",
        t.elapsed().as_secs_f64()
    );

    // -- Last SSH calls: grab kubeconfig + token, then no more SSH --
    let raw_kubeconfig = read_remote_file(
        &server_ip,
        &ssh_user,
        Some(&ssh_key_path),
        &args.known_hosts,
        "/etc/rancher/k3s/k3s.yaml",
    )
    .context("reading kubeconfig from remote control plane")?;

    let k3s_token = read_remote_file(
        &server_ip,
        &ssh_user,
        Some(&ssh_key_path),
        &args.known_hosts,
        "/var/lib/rancher/k3s/server/node-token",
    )
    .context("reading k3s node token from remote control plane")?
    .trim()
    .to_string();

    if k3s_token.is_empty() {
        bail!("k3s node token is empty");
    }

    let k3s_url = format!("https://{}:6443", server_ip);
    let kubeconfig_str = rewrite_kubeconfig_server(&raw_kubeconfig, &k3s_url);
    let registry_image_repo = format!("{}/binarylane-controller", registry_host);

    let kubeconfig_out_abs = absolute_path(&args.kubeconfig_out)?;
    fs::write(&kubeconfig_out_abs, &kubeconfig_str).with_context(|| {
        format!(
            "writing local kubeconfig to {}",
            kubeconfig_out_abs.display()
        )
    })?;
    write_docker_config(
        &docker_config_dir,
        &registry_host,
        &registry_username,
        &registry_password,
    )?;
    write_dev_tilt_values(&tilt_values_out)?;
    write_dev_resources(
        &dev_resources_out,
        &args,
        &k3s_url,
        &k3s_token,
        &cluster_name,
    )?;

    // -- From here: kube-rs only, no more SSH --
    let t = Instant::now();
    let rt = tokio::runtime::Runtime::new().context("creating tokio runtime")?;
    let kube_init_config = Arc::new(KubeInitConfig {
        registry_host: registry_host.clone(),
        registry_username: registry_username.clone(),
        registry_password: registry_password.clone(),
        registry_htpasswd: registry_htpasswd.clone(),
        registry_secret_name: args.registry_secret_name.clone(),
    });
    rt.block_on(async {
        let kubeconfig = kube::config::Kubeconfig::read_from(&kubeconfig_out_abs)
            .context("reading kubeconfig for kube client")?;
        let config = kube::Config::from_custom_kubeconfig(kubeconfig, &Default::default())
            .await
            .context("building kube config")?;
        let client = kube::Client::try_from(config).context("building kube client")?;

        // Verify API server is reachable before spawning tasks.
        wait_for_api_server(&client).await?;
        eprintln!(
            "  kube api ready .......... {:.1}s",
            t.elapsed().as_secs_f64()
        );

        let cfg = kube_init_config.clone();
        let t_taint = Instant::now();
        let t_registry = Instant::now();

        let h_taint = tokio::spawn({
            let client = client.clone();
            async move {
                remove_node_taints(&client).await?;
                eprintln!(
                    "  taint removed ........... {:.1}s",
                    t_taint.elapsed().as_secs_f64()
                );
                Ok::<(), anyhow::Error>(())
            }
        });

        let h_registry = tokio::spawn({
            let client = client.clone();
            let cfg = cfg.clone();
            async move {
                deploy_registry(&client, &cfg).await?;
                eprintln!(
                    "  registry deployed ....... {:.1}s",
                    t_registry.elapsed().as_secs_f64()
                );
                wait_for_registry_http(
                    &cfg.registry_host,
                    &cfg.registry_username,
                    &cfg.registry_password,
                )
                .await?;
                eprintln!(
                    "  registry http ready ..... {:.1}s",
                    t_registry.elapsed().as_secs_f64()
                );
                Ok::<(), anyhow::Error>(())
            }
        });

        let h_secrets = tokio::spawn({
            let client = client.clone();
            let cfg = cfg.clone();
            async move { ensure_pull_secrets(&client, &cfg).await }
        });

        let h_sa = tokio::spawn({
            let client = client.clone();
            let cfg = cfg.clone();
            async move { patch_service_accounts(&client, &cfg).await }
        });

        h_taint.await.context("taint task panicked")??;
        h_registry.await.context("registry task panicked")??;
        h_secrets.await.context("secrets task panicked")??;
        h_sa.await.context("sa task panicked")??;

        Ok::<(), anyhow::Error>(())
    })
    .context("kube cluster init failed")?;
    eprintln!(
        "  cluster init ............ {:.1}s",
        t.elapsed().as_secs_f64()
    );

    state.server_id = Some(server.id);
    state.server_name = server.name.clone();
    state.server_ip = Some(server_ip.clone());
    state.region = server.region.slug.clone();
    state.size = args.size.clone();
    state.image = args.image.clone();
    state.ssh_user = ssh_user.clone();
    state.ssh_key_path = Some(path_to_string(&ssh_key_path)?);
    state.ssh_key_id = Some(account_key.id);
    state.ssh_key_fingerprint = Some(account_key.fingerprint.clone());
    state.ssh_key_name = account_key.name.clone();
    state.registry_host = Some(registry_host.clone());
    state.registry_username = registry_username.clone();
    state.registry_password = Some(registry_password.clone());
    state.registry_pull_secret_name = args.registry_secret_name.clone();
    state.docker_config_dir = Some(path_to_string(&docker_config_dir)?);
    state.tilt_values_path = Some(path_to_string(&tilt_values_out)?);
    state.registry_image_repo = Some(registry_image_repo.clone());
    state.kubeconfig_path = Some(path_to_string(&absolute_path(&args.kubeconfig_out)?)?);
    state.known_hosts_path = Some(path_to_string(&absolute_path(&args.known_hosts)?)?);
    state.k3s_url = Some(k3s_url.clone());

    save_state(&args.state_file, &state)?;

    emit_env(
        "KUBECONFIG",
        &path_to_string(&absolute_path(&args.kubeconfig_out)?)?,
    );
    emit_env(
        "BL_DEV_STATE_FILE",
        &path_to_string(&absolute_path(&args.state_file)?)?,
    );
    emit_env("BL_API_TOKEN", &args.bl_api_token);
    emit_env("DOCKER_CONFIG", &path_to_string(&docker_config_dir)?);
    emit_env("BL_DEV_TILT_VALUES", &path_to_string(&tilt_values_out)?);
    emit_env("BL_DEV_RESOURCES", &dev_resources_out.display().to_string());
    emit_env("BL_DEV_SERVER_ID", &server.id.to_string());
    emit_env("BL_DEV_SERVER_NAME", &server.name);
    emit_env("BL_DEV_SERVER_IP", &server_ip);
    emit_env("BL_DEV_SSH_USER", &ssh_user);
    emit_env("BL_DEV_SSH_KEY_PATH", &path_to_string(&ssh_key_path)?);
    emit_env("BL_DEV_SSH_KEY_NAME", &account_key.name);
    emit_env("BL_DEV_SSH_KEY_ID", &account_key.id.to_string());
    emit_env("BL_DEV_SSH_KEY_FINGERPRINT", &account_key.fingerprint);
    emit_env("BL_DEV_REGISTRY", &registry_host);
    emit_env("BL_DEV_REGISTRY_USERNAME", &registry_username);
    emit_env("BL_DEV_REGISTRY_PASSWORD", &registry_password);
    emit_env("BL_DEV_REGISTRY_PULL_SECRET", &args.registry_secret_name);
    emit_env("BL_DEV_CONTROLLER_IMAGE", &registry_image_repo);
    emit_env("BL_DEV_AUTOSCALER_GROUP", DEV_AUTOSCALER_GROUP_ID);
    emit_env("BL_DEV_K3S_URL", &k3s_url);
    emit_env("BL_DEV_K3S_TOKEN", &k3s_token);
    emit_env("TMPL_K3S_URL", &k3s_url);
    emit_env("TMPL_K3S_TOKEN", &k3s_token);

    eprintln!(
        "  total ................... {:.1}s",
        t_total.elapsed().as_secs_f64()
    );
    Ok(())
}

fn cmd_dev_down(args: DevDownArgs) -> Result<()> {
    let state = load_state(&args.state_file)?;

    let has_cluster_name = get_or_create_cluster_name().is_ok();
    let needs_api = state.server_id.is_some() || state.ssh_key_id.is_some() || has_cluster_name;

    let client = if needs_api {
        if let Some(token) = args.bl_api_token {
            Some(BlClient::new(token)?)
        } else {
            eprintln!("Warning: BL_API_TOKEN not set; skipping remote resource cleanup");
            None
        }
    } else {
        None
    };

    if let Some(client) = &client {
        if let Some(server_id) = state.server_id {
            eprintln!("Deleting tracked server id={}...", server_id);
            let _ = client.delete_server(server_id);
        }

        if let Some(ssh_key_id) = state.ssh_key_id {
            eprintln!("Deleting tracked account SSH key id={}...", ssh_key_id);
            let _ = client.delete_account_ssh_key(ssh_key_id);
        }

        if let Ok(cluster_name) = get_or_create_cluster_name() {
            let prefix = format!("{cluster_name}-");

            if let Ok(servers) = client.list_servers() {
                for s in servers {
                    if s.name.starts_with(&prefix) {
                        eprintln!(
                            "Deleting prefix-matched server '{}' (id={})...",
                            s.name, s.id
                        );
                        let _ = client.delete_server(s.id);
                    }
                }
            }

            if let Ok(keys) = client.list_account_ssh_keys() {
                for key in keys {
                    if key.name.starts_with(&prefix) {
                        eprintln!(
                            "Deleting prefix-matched account SSH key '{}' (id={})...",
                            key.name, key.id
                        );
                        let _ = client.delete_account_ssh_key(key.id);
                    }
                }
            }

            if let Ok(lbs) = client.list_load_balancers() {
                for lb in lbs {
                    if lb.name.starts_with(&prefix) {
                        eprintln!(
                            "Deleting prefix-matched load balancer '{}' (id={})...",
                            lb.name, lb.id
                        );
                        let _ = client.delete_load_balancer(lb.id);
                    }
                }
            }
        }
    }

    remove_file_if_exists(&args.kubeconfig_out)?;

    remove_file_if_exists(&args.known_hosts)?;

    remove_file_if_exists(&args.tilt_values_out)?;
    remove_file_if_exists(Path::new(".dev/name"))?;

    let default_key = Path::new(DEFAULT_MANAGED_SSH_KEY_PATH);
    remove_file_if_exists(default_key)?;
    remove_file_if_exists(&managed_public_key_path(default_key))?;

    if let Some(path) = &state.kubeconfig_path {
        remove_file_if_exists(Path::new(path))?;
    }
    if let Some(path) = &state.known_hosts_path {
        remove_file_if_exists(Path::new(path))?;
    }
    if let Some(path) = &state.tilt_values_path {
        remove_file_if_exists(Path::new(path))?;
    }
    if let Some(path) = &state.ssh_key_path {
        let private_path = Path::new(path);
        remove_file_if_exists(private_path)?;
        let public_path = managed_public_key_path(private_path);
        remove_file_if_exists(&public_path)?;
    }
    remove_dir_if_exists(&args.docker_config_dir)?;
    if let Some(path) = &state.docker_config_dir {
        remove_dir_if_exists(Path::new(path))?;
    }

    remove_file_if_exists(&args.state_file)?;

    eprintln!("Local dev state cleaned up");
    Ok(())
}

fn cmd_tilt(args: TiltArgs) -> Result<()> {
    ensure_tool("tilt", "install Tilt: https://docs.tilt.dev/install.html")?;

    let mut cmd = Command::new("tilt");

    // Default to `up` if no subcommand given (or first arg is a flag).
    let has_subcommand = args.args.first().is_some_and(|a| !a.starts_with('-'));
    if !has_subcommand {
        cmd.arg("up");
    }

    // --port 0 lets the OS pick a free port, avoiding collisions when
    // multiple Tilt instances run across different projects.
    cmd.arg("--port").arg("0");
    cmd.args(&args.args);

    let status = cmd.status().context("running tilt")?;
    std::process::exit(status.code().unwrap_or(1));
}

fn create_dev_server(
    client: &BlClient,
    args: &DevUpArgs,
    server_name: &str,
    ssh_key_name: &str,
    ssh_key_fingerprint: &str,
) -> Result<Server> {
    let server_password = generate_server_password();

    let mut regions_to_try = vec![args.region.as_str()];
    for r in REGION_FALLBACKS {
        if *r != args.region {
            regions_to_try.push(r);
        }
    }

    for (i, region) in regions_to_try.iter().enumerate() {
        eprintln!(
            "Creating dev control plane server '{}' (region={}, size={}, image={}, ssh_key={})",
            server_name, region, args.size, args.image, ssh_key_name
        );

        match client.create_server(CreateServerRequest {
            name: server_name.to_string(),
            size: args.size.clone(),
            image: args.image.clone(),
            region: region.to_string(),
            user_data: None,
            ssh_keys: Some(vec![ssh_key_fingerprint.to_string()]),
            password: Some(server_password.clone()),
        }) {
            Ok(server) => {
                eprintln!("Created server id={} name={}", server.id, server.name);
                return Ok(server);
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("Unable to find a suitable host") && i + 1 < regions_to_try.len() {
                    eprintln!(
                        "Region '{}' has no capacity, trying '{}'...",
                        region,
                        regions_to_try[i + 1]
                    );
                    continue;
                }
                return Err(e).context("creating BinaryLane server");
            }
        }
    }

    bail!("no region had available capacity")
}

fn wait_for_server_active(client: &BlClient, server_id: i64, timeout: Duration) -> Result<Server> {
    eprintln!("Waiting for server id={} to become active...", server_id);
    let start = Instant::now();
    let mut last_status = String::new();

    loop {
        let server = client
            .get_server(server_id)?
            .ok_or_else(|| anyhow::anyhow!("server id={} disappeared while waiting", server_id))?;

        if server.status != last_status {
            eprintln!(
                "Server id={} status={} ({:.0}s)",
                server_id,
                server.status,
                start.elapsed().as_secs_f64()
            );
            last_status = server.status.clone();
        }

        if server.status == "active" {
            return Ok(server);
        }

        if start.elapsed() > timeout {
            bail!(
                "timed out waiting for server id={} to become active (last status={})",
                server_id,
                server.status
            );
        }

        thread::sleep(Duration::from_secs(5));
    }
}

fn wait_for_ssh_ready(
    host: &str,
    users: &[String],
    ssh_key_path: Option<&Path>,
    known_hosts: &Path,
    timeout: Duration,
) -> Result<String> {
    eprintln!("Waiting for SSH on {}...", host);
    let start = Instant::now();
    let mut attempts = 0u64;

    loop {
        attempts += 1;
        // Enable verbose SSH output every 6th attempt so we can diagnose failures.
        let verbose = attempts == 1 || attempts.is_multiple_of(6);
        for user in users {
            if ssh_probe(host, user, ssh_key_path, known_hosts, verbose)? {
                eprintln!("SSH is reachable on {} as {}", host, user);
                return Ok(user.clone());
            }
        }

        if verbose {
            eprintln!(
                "Still waiting for SSH on {} (tried users: {}, {:.0}s elapsed)",
                host,
                users.join(", "),
                start.elapsed().as_secs_f64(),
            );
        }

        if start.elapsed() > timeout {
            bail!(
                "timed out waiting for SSH on {} (tried users: {})",
                host,
                users.join(", ")
            );
        }

        thread::sleep(Duration::from_secs(5));
    }
}

fn ensure_k3s_server(
    host: &str,
    user: &str,
    ssh_key_path: Option<&Path>,
    known_hosts: &Path,
    registry: &K3sRegistryConfig<'_>,
    timeout: Duration,
    node_name: &str,
) -> Result<()> {
    eprintln!("Installing/verifying k3s control plane on {}...", host);
    let start = Instant::now();
    let k3s_url = format!("https://{}:6443", host);
    let registries_yaml = format!(
        "mirrors:\n  \"{registry_host}\":\n    endpoint:\n      - \"http://{registry_host}\"\nconfigs:\n  \"{registry_host}\":\n    auth:\n      username: {registry_username}\n      password: {registry_password}\n",
        registry_host = registry.host,
        registry_username = yaml_escape(registry.username),
        registry_password = yaml_escape(registry.password),
    );
    let install_script = format!(
        "set -eu\n\
if command -v cloud-init >/dev/null 2>&1; then\n\
  cloud-init status --wait || true\n\
fi\n\
if [ \"$(id -u)\" -eq 0 ]; then\n\
  SUDO=\"\"\n\
else\n\
  SUDO=\"sudo\"\n\
fi\n\
${{SUDO}} mkdir -p /etc/rancher/k3s\n\
DESIRED_REGISTRIES=$(cat <<'EOF_REGISTRIES'\n\
{registries_yaml}\
EOF_REGISTRIES\n\
)\n\
registries_changed=0\n\
CURRENT_REGISTRIES=$(${{SUDO}} cat /etc/rancher/k3s/registries.yaml 2>/dev/null || true)\n\
if [ \"$DESIRED_REGISTRIES\" != \"$CURRENT_REGISTRIES\" ]; then\n\
  printf '%s' \"$DESIRED_REGISTRIES\" | ${{SUDO}} tee /etc/rancher/k3s/registries.yaml >/dev/null\n\
  registries_changed=1\n\
fi\n\
k3s_was_active=0\n\
if ${{SUDO}} systemctl is-active --quiet k3s; then\n\
  k3s_was_active=1\n\
fi\n\
if ! command -v curl >/dev/null 2>&1; then\n\
  if command -v apt-get >/dev/null 2>&1; then\n\
    ${{SUDO}} apt-get update -y\n\
    ${{SUDO}} apt-get install -y curl\n\
  elif command -v dnf >/dev/null 2>&1; then\n\
    ${{SUDO}} dnf install -y curl\n\
  elif command -v apk >/dev/null 2>&1; then\n\
    ${{SUDO}} apk add --no-cache curl\n\
  fi\n\
fi\n\
if ! command -v k3s >/dev/null 2>&1; then\n\
  curl -sfL https://get.k3s.io | ${{SUDO}} env INSTALL_K3S_EXEC='server --write-kubeconfig-mode 644 --disable traefik --disable-cloud-controller --kubelet-arg=cloud-provider=external --tls-san {host} --node-name {node_name}' sh -s -\n\
elif [ \"$k3s_was_active\" -eq 1 ] && [ \"$registries_changed\" -eq 1 ]; then\n\
  ${{SUDO}} systemctl restart k3s\n\
fi\n\
${{SUDO}} systemctl enable --now k3s >/dev/null 2>&1 || true\n\
${{SUDO}} systemctl is-active --quiet k3s\n",
        registries_yaml = registries_yaml,
        node_name = node_name,
    );

    loop {
        match run_ssh_script(
            host,
            user,
            ssh_key_path,
            known_hosts,
            &install_script,
            SshOutputMode::Forward,
        ) {
            Ok(_) => {
                eprintln!("k3s ready at {}", k3s_url);
                return Ok(());
            }
            Err(err) => {
                if start.elapsed() > timeout {
                    return Err(err).context("timed out installing/verifying k3s");
                }
                eprintln!("k3s not ready yet: {err}");
                thread::sleep(Duration::from_secs(8));
            }
        }
    }
}

struct KubeInitConfig {
    registry_host: String,
    registry_username: String,
    registry_password: String,
    registry_htpasswd: String,
    registry_secret_name: String,
}

struct K3sRegistryConfig<'a> {
    host: &'a str,
    username: &'a str,
    password: &'a str,
}

async fn wait_for_api_server(client: &kube::Client) -> Result<()> {
    let nodes: Api<Node> = Api::all(client.clone());
    for attempt in 1u32.. {
        match nodes.list(&Default::default()).await {
            Ok(list) if !list.items.is_empty() => return Ok(()),
            Ok(_) => {
                if attempt > 60 {
                    bail!("timed out waiting for nodes to register");
                }
                if attempt == 1 || attempt % 5 == 0 {
                    eprintln!("Waiting for nodes to register...");
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(e) => {
                if attempt > 60 {
                    bail!("timed out waiting for kube API server: {e:#}");
                }
                if attempt == 1 || attempt % 5 == 0 {
                    eprintln!("Waiting for kube API server ({e})...");
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    unreachable!()
}

async fn remove_node_taints(client: &kube::Client) -> Result<()> {
    let nodes: Api<Node> = Api::all(client.clone());
    let node_list = nodes
        .list(&Default::default())
        .await
        .context("listing nodes")?;
    for node in &node_list {
        let name = node.name_any();
        let taints = node
            .spec
            .as_ref()
            .and_then(|s| s.taints.as_ref())
            .cloned()
            .unwrap_or_default();
        let has_taint = taints
            .iter()
            .any(|t| t.key == "node.cloudprovider.kubernetes.io/uninitialized");
        if !has_taint {
            continue;
        }
        let new_taints: Vec<serde_json::Value> = taints
            .iter()
            .filter(|t| t.key != "node.cloudprovider.kubernetes.io/uninitialized")
            .map(|t| {
                let mut v = serde_json::json!({
                    "key": t.key,
                    "effect": t.effect,
                });
                if let Some(val) = &t.value {
                    v["value"] = serde_json::json!(val);
                }
                if let Some(ts) = &t.time_added {
                    v["timeAdded"] = serde_json::json!(ts.0.to_rfc3339());
                }
                v
            })
            .collect();
        // Use strategic merge patch via application/strategic-merge-patch+json.
        // For taints (merge key: "key"), setting the list replaces it entirely
        // when using a regular merge patch.
        let patch = serde_json::json!({ "spec": { "taints": new_taints } });
        nodes
            .patch(&name, &PatchParams::default(), &Patch::Merge(&patch))
            .await
            .with_context(|| format!("removing taint from node {name}"))?;
        // Verify
        let updated = nodes
            .get(&name)
            .await
            .context("re-reading node after taint patch")?;
        let still_tainted = updated
            .spec
            .as_ref()
            .and_then(|s| s.taints.as_ref())
            .is_some_and(|t| {
                t.iter()
                    .any(|t| t.key == "node.cloudprovider.kubernetes.io/uninitialized")
            });
        if still_tainted {
            bail!("taint still present on node {name} after patch");
        }
        eprintln!("Removed uninitialized taint from node {name}");
    }
    Ok(())
}

async fn deploy_registry(client: &kube::Client, cfg: &KubeInitConfig) -> Result<()> {
    let pp = PatchParams::apply("xtask").force();

    // Namespace
    let ns_api: Api<Namespace> = Api::all(client.clone());
    ns_api
        .patch(
            REGISTRY_NAMESPACE,
            &pp,
            &Patch::Apply(serde_json::json!({
                "apiVersion": "v1",
                "kind": "Namespace",
                "metadata": { "name": REGISTRY_NAMESPACE }
            })),
        )
        .await
        .context("applying registry namespace")?;

    // Auth secret
    let secrets: Api<Secret> = Api::namespaced(client.clone(), REGISTRY_NAMESPACE);
    secrets
        .patch(
            "registry-auth",
            &pp,
            &Patch::Apply(serde_json::json!({
                "apiVersion": "v1",
                "kind": "Secret",
                "metadata": { "name": "registry-auth", "namespace": REGISTRY_NAMESPACE },
                "type": "Opaque",
                "stringData": { "htpasswd": cfg.registry_htpasswd }
            })),
        )
        .await
        .context("applying registry auth secret")?;

    // Deployment (no toleration — taint is removed by parallel task)
    let deploys: Api<Deployment> = Api::namespaced(client.clone(), REGISTRY_NAMESPACE);
    deploys
        .patch(
            "registry",
            &pp,
            &Patch::Apply(serde_json::json!({
                "apiVersion": "apps/v1",
                "kind": "Deployment",
                "metadata": { "name": "registry", "namespace": REGISTRY_NAMESPACE },
                "spec": {
                    "replicas": 1,
                    "selector": { "matchLabels": { "app": "registry" } },
                    "template": {
                        "metadata": { "labels": { "app": "registry" } },
                        "spec": {
                            "containers": [{
                                "name": "registry",
                                "image": "registry:2",
                                "imagePullPolicy": "IfNotPresent",
                                "env": [
                                    { "name": "REGISTRY_AUTH", "value": "htpasswd" },
                                    { "name": "REGISTRY_AUTH_HTPASSWD_REALM", "value": "dev" },
                                    { "name": "REGISTRY_AUTH_HTPASSWD_PATH", "value": "/auth/htpasswd" },
                                ],
                                "ports": [{ "containerPort": 5000, "name": "http" }],
                                "volumeMounts": [
                                    { "name": "data", "mountPath": "/var/lib/registry" },
                                    { "name": "auth", "mountPath": "/auth/htpasswd", "subPath": "htpasswd" },
                                ],
                                "securityContext": { "runAsUser": 0, "runAsGroup": 0 },
                            }],
                            "volumes": [
                                { "name": "data", "hostPath": { "path": REGISTRY_DATA_HOSTPATH, "type": "DirectoryOrCreate" } },
                                { "name": "auth", "secret": { "secretName": "registry-auth" } },
                            ],
                        }
                    }
                }
            })),
        )
        .await
        .context("applying registry deployment")?;

    // Service
    let svcs: Api<Service> = Api::namespaced(client.clone(), REGISTRY_NAMESPACE);
    svcs.patch(
        "registry",
        &pp,
        &Patch::Apply(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": { "name": "registry", "namespace": REGISTRY_NAMESPACE },
            "spec": {
                "type": "NodePort",
                "selector": { "app": "registry" },
                "ports": [{
                    "name": "http",
                    "port": 5000,
                    "targetPort": 5000,
                    "nodePort": 30500,
                }]
            }
        })),
    )
    .await
    .context("applying registry service")?;

    // Wait for rollout
    for attempt in 1u32.. {
        let deploy = deploys
            .get("registry")
            .await
            .context("getting registry deployment")?;
        let status = deploy.status.as_ref();
        let ready = status.and_then(|s| s.ready_replicas).unwrap_or(0);
        let replicas = status.and_then(|s| s.replicas).unwrap_or(0);
        let unavailable = status.and_then(|s| s.unavailable_replicas).unwrap_or(0);
        if ready >= 1 {
            return Ok(());
        }
        if attempt % 10 == 1 {
            eprintln!(
                "Registry rollout: replicas={replicas} ready={ready} unavailable={unavailable} ({attempt}s)"
            );
        }
        if attempt > 300 {
            bail!("timed out waiting for registry deployment rollout");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    unreachable!()
}

async fn wait_for_registry_http(
    registry_host: &str,
    registry_username: &str,
    registry_password: &str,
) -> Result<()> {
    eprintln!(
        "Waiting for registry endpoint http://{}/v2/ ...",
        registry_host
    );
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("building HTTP client for registry probe")?;

    let start = Instant::now();
    loop {
        let resp = http
            .get(format!("http://{registry_host}/v2/"))
            .basic_auth(registry_username, Some(registry_password))
            .send()
            .await;

        match resp {
            Ok(resp) if resp.status().is_success() => return Ok(()),
            Ok(resp) => {
                eprintln!(
                    "Registry probe: status {} ({:.0}s)",
                    resp.status(),
                    start.elapsed().as_secs_f64()
                );
            }
            Err(e) => {
                eprintln!(
                    "Registry probe: {} ({:.0}s)",
                    e,
                    start.elapsed().as_secs_f64()
                );
            }
        }

        if start.elapsed() > Duration::from_secs(300) {
            bail!(
                "timed out waiting for registry endpoint http://{}/v2/",
                registry_host
            );
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn ensure_pull_secrets(client: &kube::Client, cfg: &KubeInitConfig) -> Result<()> {
    let docker_config = serde_json::json!({
        "auths": {
            &cfg.registry_host: {
                "username": &cfg.registry_username,
                "password": &cfg.registry_password,
            }
        }
    });
    let docker_config_bytes = serde_json::to_vec(&docker_config)?;

    let pp = PatchParams::apply("xtask").force();
    for ns_name in [REGISTRY_NAMESPACE, "default", "binarylane-system"] {
        // Ensure namespace
        let ns_api: Api<Namespace> = Api::all(client.clone());
        ns_api
            .patch(
                ns_name,
                &pp,
                &Patch::Apply(serde_json::json!({
                    "apiVersion": "v1",
                    "kind": "Namespace",
                    "metadata": { "name": ns_name }
                })),
            )
            .await
            .with_context(|| format!("ensuring namespace {ns_name}"))?;

        // Apply docker-registry secret
        let secrets: Api<Secret> = Api::namespaced(client.clone(), ns_name);
        secrets
            .patch(
                &cfg.registry_secret_name,
                &pp,
                &Patch::Apply(serde_json::json!({
                    "apiVersion": "v1",
                    "kind": "Secret",
                    "metadata": { "name": &cfg.registry_secret_name, "namespace": ns_name },
                    "type": "kubernetes.io/dockerconfigjson",
                    "data": {
                        ".dockerconfigjson": base64::engine::general_purpose::STANDARD.encode(&docker_config_bytes),
                    }
                })),
            )
            .await
            .with_context(|| format!("applying pull secret in {ns_name}"))?;
    }
    Ok(())
}

async fn patch_service_accounts(client: &kube::Client, cfg: &KubeInitConfig) -> Result<()> {
    let sa_api: Api<ServiceAccount> = Api::namespaced(client.clone(), "default");
    // Wait for default SA to exist
    for attempt in 1u32.. {
        match sa_api.get("default").await {
            Ok(_) => break,
            Err(_) if attempt > 60 => bail!("timed out waiting for default service account"),
            Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }
    let patch = serde_json::json!({
        "imagePullSecrets": [{ "name": &cfg.registry_secret_name }]
    });
    sa_api
        .patch("default", &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .context("patching default service account")?;
    Ok(())
}

fn read_remote_file(
    host: &str,
    user: &str,
    ssh_key_path: Option<&Path>,
    known_hosts: &Path,
    file_path: &str,
) -> Result<String> {
    let quoted_path = shell_single_quote(file_path);
    let script = format!(
        "set -eu\n\
if [ \"$(id -u)\" -eq 0 ]; then\n\
  cat {quoted_path}\n\
else\n\
  sudo cat {quoted_path}\n\
fi\n"
    );

    run_ssh_script(
        host,
        user,
        ssh_key_path,
        known_hosts,
        &script,
        SshOutputMode::Capture,
    )
}

fn ssh_probe(
    host: &str,
    user: &str,
    ssh_key_path: Option<&Path>,
    known_hosts: &Path,
    verbose: bool,
) -> Result<bool> {
    let mut cmd = ssh_base_command(host, user, ssh_key_path, known_hosts);
    if verbose {
        cmd.args(["-o", "LogLevel=DEBUG"]);
    }
    cmd.arg("true");

    let child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning SSH probe")?;

    // Hard 30s deadline: catches cases where ConnectTimeout doesn't cover the
    // full handshake (e.g. middlebox holding the TCP connection open).
    let output = child.wait_with_output().context("waiting for SSH probe")?;

    if !output.status.success() && verbose {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.trim().is_empty() {
            eprintln!("ssh probe {}@{}: {}", user, host, stderr.trim());
        }
    }
    Ok(output.status.success())
}

#[derive(Copy, Clone)]
enum SshOutputMode {
    Forward,
    Capture,
}

fn run_ssh_script(
    host: &str,
    user: &str,
    ssh_key_path: Option<&Path>,
    known_hosts: &Path,
    script: &str,
    mode: SshOutputMode,
) -> Result<String> {
    let mut cmd = ssh_base_command(host, user, ssh_key_path, known_hosts);
    cmd.arg("sh").arg("-s");
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().context("starting SSH process")?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .context("opening SSH stdin for script")?;
        stdin
            .write_all(script.as_bytes())
            .context("writing remote script to SSH stdin")?;
    }

    let output = child
        .wait_with_output()
        .context("waiting for SSH command")?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !output.status.success() {
        if !stdout.trim().is_empty() {
            eprintln!("{stdout}");
        }
        if !stderr.trim().is_empty() {
            eprintln!("{stderr}");
        }
        bail!("remote SSH command failed with status {}", output.status);
    }

    match mode {
        SshOutputMode::Forward => {
            if !stdout.trim().is_empty() {
                eprintln!("{stdout}");
            }
            if !stderr.trim().is_empty() {
                eprintln!("{stderr}");
            }
            Ok(String::new())
        }
        SshOutputMode::Capture => Ok(stdout),
    }
}

fn ssh_base_command(
    host: &str,
    user: &str,
    ssh_key_path: Option<&Path>,
    known_hosts: &Path,
) -> Command {
    let mut cmd = Command::new("ssh");
    let known_hosts_opt = format!("UserKnownHostsFile={}", known_hosts.display());
    cmd.args([
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=accept-new",
        "-o",
        &known_hosts_opt,
    ])
    .args([
        "-o",
        "LogLevel=ERROR",
        "-o",
        "ConnectTimeout=5",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
    ])
    // Prevent hangs from GSSAPI negotiation or a broken SSH agent socket
    // (common in devcontainers / Codespaces).
    .args(["-o", "GSSAPIAuthentication=no", "-o", "IdentityAgent=none"]);

    if let Some(key) = ssh_key_path {
        cmd.args(["-o", "IdentitiesOnly=yes", "-i"]).arg(key);
    }

    cmd.arg(format!("{user}@{host}"));
    cmd
}

fn rewrite_kubeconfig_server(kubeconfig: &str, k3s_url: &str) -> String {
    let out = kubeconfig
        .replace("https://127.0.0.1:6443", k3s_url)
        .replace("https://localhost:6443", k3s_url);
    if out == kubeconfig {
        eprintln!("Warning: kubeconfig server endpoint did not need rewrite");
    }
    out
}

fn ensure_managed_ssh_keypair(private_key_path: &Path) -> Result<()> {
    ensure_parent_dir(private_key_path)?;
    let public_key_path = managed_public_key_path(private_key_path);

    if private_key_path.exists() && public_key_path.exists() {
        return Ok(());
    }

    if private_key_path.exists() && !public_key_path.exists() {
        eprintln!(
            "Managed SSH private key exists, regenerating missing public key: {}",
            public_key_path.display()
        );
        let output = Command::new("ssh-keygen")
            .args(["-y", "-f"])
            .arg(private_key_path)
            .output()
            .context("rebuilding managed SSH public key")?;
        if !output.status.success() {
            bail!(
                "ssh-keygen failed to rebuild public key: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        fs::write(&public_key_path, output.stdout).with_context(|| {
            format!(
                "writing managed SSH public key {}",
                public_key_path.display()
            )
        })?;
        return Ok(());
    }

    eprintln!(
        "Generating managed SSH keypair at {}",
        private_key_path.display()
    );
    let output = Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-f"])
        .arg(private_key_path)
        .args(["-C", "binarylane-controller-dev"])
        .output()
        .context("generating managed SSH keypair")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.trim().is_empty() {
        eprintln!("{stdout}");
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        eprintln!("{stderr}");
    }
    if !output.status.success() {
        bail!("ssh-keygen failed with status {}", output.status);
    }
    // Ensure restrictive permissions — some filesystems (e.g. Codespaces /workspaces)
    // default to 0644 which SSH rejects.
    fs::set_permissions(private_key_path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("setting permissions on {}", private_key_path.display()))?;
    Ok(())
}

fn managed_public_key_path(private_key_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.pub", private_key_path.display()))
}

fn read_managed_public_key(private_key_path: &Path) -> Result<String> {
    let public_key_path = managed_public_key_path(private_key_path);
    let content = fs::read_to_string(&public_key_path).with_context(|| {
        format!(
            "reading managed SSH public key {}",
            public_key_path.display()
        )
    })?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        bail!(
            "managed SSH public key {} is empty",
            public_key_path.display()
        );
    }
    Ok(trimmed.to_string())
}

fn normalize_public_key(public_key: &str) -> &str {
    public_key.trim()
}

fn ensure_account_ssh_key(
    client: &BlClient,
    name: &str,
    public_key: &str,
    preferred_id: Option<i64>,
    preferred_fingerprint: Option<&str>,
) -> Result<AccountSshKey> {
    let keys = client.list_account_ssh_keys()?;

    let mut candidate = preferred_id.and_then(|id| keys.iter().find(|k| k.id == id).cloned());
    if candidate.is_none()
        && let Some(fp) = preferred_fingerprint
    {
        candidate = keys.iter().find(|k| k.fingerprint == fp).cloned();
    }
    if candidate.is_none() {
        candidate = keys.iter().find(|k| k.name == name).cloned();
    }

    if let Some(existing) = candidate {
        if normalize_public_key(&existing.public_key) == normalize_public_key(public_key) {
            eprintln!(
                "Reusing account SSH key '{}' (id={}, fingerprint={})",
                existing.name, existing.id, existing.fingerprint
            );
            return Ok(existing);
        }

        eprintln!(
            "Replacing account SSH key '{}' (id={}) because public key changed",
            existing.name, existing.id
        );
        client.delete_account_ssh_key(existing.id)?;
    }

    eprintln!("Creating account SSH key '{}'", name);
    let created = client.create_account_ssh_key(CreateAccountSshKeyRequest {
        name: name.to_string(),
        public_key: public_key.to_string(),
    })?;
    eprintln!(
        "Created account SSH key '{}' (id={}, fingerprint={})",
        created.name, created.id, created.fingerprint
    );
    Ok(created)
}

fn load_state(path: &Path) -> Result<DevState> {
    match fs::read_to_string(path) {
        Ok(data) => serde_json::from_str(&data)
            .with_context(|| format!("parsing state file {}", path.display())),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(DevState::default()),
        Err(err) => Err(err).with_context(|| format!("reading state file {}", path.display())),
    }
}

fn save_state(path: &Path, state: &DevState) -> Result<()> {
    ensure_parent_dir(path)?;
    let json = serde_json::to_string_pretty(state).context("serializing state file")?;
    fs::write(path, format!("{json}\n"))
        .with_context(|| format!("writing state file {}", path.display()))
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(_) => {
            eprintln!("Removed {}", path.display());
            Ok(())
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
    }
}

fn remove_dir_if_exists(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(_) => {
            eprintln!("Removed {}", path.display());
            Ok(())
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let cwd = std::env::current_dir().context("getting current directory")?;
    Ok(cwd.join(path))
}

fn path_to_string(path: &Path) -> Result<String> {
    path.to_str()
        .map(ToString::to_string)
        .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8: {}", path.display()))
}

fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    Ok(())
}

fn ensure_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("creating directory {}", path.display()))
}

fn ensure_tool(name: &str, install_hint: &str) -> Result<()> {
    let found = Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !found {
        bail!("'{name}' not found in PATH ({install_hint})");
    }
    Ok(())
}

fn write_docker_config(
    docker_config_dir: &Path,
    registry_host: &str,
    registry_username: &str,
    registry_password: &str,
) -> Result<()> {
    ensure_dir(docker_config_dir)?;
    let auth = base64::engine::general_purpose::STANDARD
        .encode(format!("{registry_username}:{registry_password}"));
    let config = serde_json::json!({
        "auths": {
            registry_host: {
                "auth": auth,
                "username": registry_username,
                "password": registry_password,
            }
        }
    });
    let content = serde_json::to_string_pretty(&config).context("serializing docker config")?;
    fs::write(
        docker_config_dir.join("config.json"),
        format!("{content}\n"),
    )
    .with_context(|| {
        format!(
            "writing docker auth config to {}",
            docker_config_dir.join("config.json").display()
        )
    })
}

fn write_dev_tilt_values(tilt_values_path: &Path) -> Result<()> {
    ensure_parent_dir(tilt_values_path)?;

    let contents = r#"autoscaler:
  enabled: true
  listenAddr: "0.0.0.0:8086"
mtls:
  enabled: true
secretNamespaces:
  - default
"#
    .to_string();

    fs::write(tilt_values_path, contents).with_context(|| {
        format!(
            "writing generated tilt values {}",
            tilt_values_path.display()
        )
    })
}

fn write_dev_resources(
    resources_path: &Path,
    args: &DevUpArgs,
    k3s_url: &str,
    k3s_token: &str,
    cluster_name: &str,
) -> Result<()> {
    ensure_parent_dir(resources_path)?;

    let cloud_init_template =
        fs::read_to_string("dev-cloud-init.sh").context("reading dev-cloud-init.sh")?;
    let cloud_init = cloud_init_template
        .replace("{{.K3S_URL}}", k3s_url)
        .replace("{{.K3S_TOKEN}}", k3s_token)
        .replace("{{.NodeName}}", "$(hostname)")
        .replace("{{.NodeGroup}}", DEV_AUTOSCALER_GROUP_ID);

    let contents = format!(
        r#"apiVersion: v1
kind: Secret
metadata:
  name: dev-cloud-init
  namespace: default
type: Opaque
stringData:
  user-data: |
{cloud_init}---
apiVersion: blc.samcday.com/v1alpha1
kind: AutoScalingGroup
metadata:
  name: {group_id}
spec:
  minSize: 0
  maxSize: 3
  size: "{size}"
  region: "{region}"
  image: "{image}"
  namePrefix: "{cluster_name}-"
  userDataSecretRef:
    name: dev-cloud-init
    namespace: default
    key: user-data
"#,
        cloud_init = indent_block(&cloud_init, 4),
        group_id = DEV_AUTOSCALER_GROUP_ID,
        size = yaml_escape(&args.size),
        region = yaml_escape(&args.region),
        image = yaml_escape(&args.image),
        cluster_name = cluster_name,
    );

    fs::write(resources_path, contents)
        .with_context(|| format!("writing dev resources {}", resources_path.display()))
}

fn yaml_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn indent_block(value: &str, spaces: usize) -> String {
    let prefix = " ".repeat(spaces);
    let mut out = String::new();
    for line in value.lines() {
        out.push_str(&prefix);
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn generate_registry_password() -> String {
    thread_rng()
        .sample_iter(Alphanumeric)
        .take(40)
        .map(char::from)
        .collect()
}

fn generate_server_password() -> String {
    binarylane_client::generate_server_password()
}

fn shell_single_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    let mut out = String::from("'");
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

fn emit_env(key: &str, value: &str) {
    println!("export {key}={}", shell_single_quote(value));
}

impl Server {
    fn public_ipv4(&self) -> Option<String> {
        self.networks
            .v4
            .iter()
            .find(|n| n.net_type == "public")
            .map(|n| n.ip_address.clone())
    }
}

impl BlClient {
    fn new(token: String) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            token,
            api_base: DEFAULT_API_BASE.to_string(),
            http,
        })
    }

    fn get_server(&self, server_id: i64) -> Result<Option<Server>> {
        let resp = self
            .request(reqwest::Method::GET, &format!("/servers/{server_id}"))
            .send()
            .context("getting server")?;

        if resp.status().as_u16() == 404 {
            return Ok(None);
        }

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            bail!("getting server {server_id}: {status}: {body}");
        }

        #[derive(Deserialize)]
        struct Resp {
            server: Server,
        }
        let body: Resp = resp.json().context("decoding get server response")?;
        Ok(Some(body.server))
    }

    fn create_server(&self, req: CreateServerRequest) -> Result<Server> {
        let resp = self
            .request(reqwest::Method::POST, "/servers")
            .json(&req)
            .send()
            .context("creating server")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            bail!("creating server: {status}: {body}");
        }

        #[derive(Deserialize)]
        struct Resp {
            server: Server,
        }
        let body: Resp = resp.json().context("decoding create server response")?;
        Ok(body.server)
    }

    fn delete_server(&self, server_id: i64) -> Result<()> {
        let resp = self
            .request(reqwest::Method::DELETE, &format!("/servers/{server_id}"))
            .send()
            .context("deleting server")?;

        if resp.status().as_u16() == 404 {
            return Ok(());
        }

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            bail!("deleting server {server_id}: {status}: {body}");
        }
        Ok(())
    }

    fn list_account_ssh_keys(&self) -> Result<Vec<AccountSshKey>> {
        let resp = self
            .request(reqwest::Method::GET, "/account/keys")
            .send()
            .context("listing account SSH keys")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            bail!("listing account SSH keys: {status}: {body}");
        }

        #[derive(Deserialize)]
        struct Resp {
            ssh_keys: Vec<AccountSshKey>,
        }
        let body: Resp = resp
            .json()
            .context("decoding list account SSH keys response")?;
        Ok(body.ssh_keys)
    }

    fn list_servers(&self) -> Result<Vec<Server>> {
        let resp = self
            .request(reqwest::Method::GET, "/servers")
            .send()
            .context("listing servers")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            bail!("listing servers: {status}: {body}");
        }

        #[derive(Deserialize)]
        struct Resp {
            servers: Vec<Server>,
        }
        let body: Resp = resp.json().context("decoding list servers response")?;
        Ok(body.servers)
    }

    fn list_load_balancers(&self) -> Result<Vec<LoadBalancer>> {
        let resp = self
            .request(reqwest::Method::GET, "/load_balancers")
            .send()
            .context("listing load balancers")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            bail!("listing load balancers: {status}: {body}");
        }

        #[derive(Deserialize)]
        struct Resp {
            load_balancers: Vec<LoadBalancer>,
        }
        let body: Resp = resp
            .json()
            .context("decoding list load balancers response")?;
        Ok(body.load_balancers)
    }

    fn delete_load_balancer(&self, lb_id: i64) -> Result<()> {
        let resp = self
            .request(
                reqwest::Method::DELETE,
                format!("/load_balancers/{} ", lb_id).trim(),
            )
            .send()
            .context("deleting load balancer")?;

        if resp.status().as_u16() == 404 {
            return Ok(());
        }

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            bail!("deleting load balancer {lb_id}: {status}: {body}");
        }
        Ok(())
    }

    fn create_account_ssh_key(&self, req: CreateAccountSshKeyRequest) -> Result<AccountSshKey> {
        let resp = self
            .request(reqwest::Method::POST, "/account/keys")
            .json(&req)
            .send()
            .context("creating account SSH key")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            bail!("creating account SSH key: {status}: {body}");
        }

        #[derive(Deserialize)]
        struct Resp {
            ssh_key: AccountSshKey,
        }
        let body: Resp = resp
            .json()
            .context("decoding create account SSH key response")?;
        Ok(body.ssh_key)
    }

    fn delete_account_ssh_key(&self, key_id: i64) -> Result<()> {
        let resp = self
            .request(reqwest::Method::DELETE, &format!("/account/keys/{key_id}"))
            .send()
            .context("deleting account SSH key")?;

        if resp.status().as_u16() == 404 {
            return Ok(());
        }

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            bail!("deleting account SSH key {key_id}: {status}: {body}");
        }
        Ok(())
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::blocking::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.api_base))
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
    }
}

fn get_or_create_cluster_name() -> Result<String> {
    let path = std::path::Path::new(".dev/name");
    if path.exists() {
        let name = std::fs::read_to_string(path)?;
        return Ok(name.trim().to_string());
    }
    std::fs::create_dir_all(".dev")?;
    let hash: String = rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(6)
        .map(char::from)
        .collect();
    let name = format!("bl-dev-{}", hash.to_lowercase());
    std::fs::write(path, &name)?;
    Ok(name)
}
