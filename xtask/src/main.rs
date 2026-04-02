use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use base64::Engine;
use bcrypt::hash;
use clap::{Args, Parser, Subcommand};
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

const DEFAULT_REGION: &str = "syd";
const DEFAULT_SIZE: &str = "std-min";
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
    /// Instance name for parallel dev environments (e.g. CI matrix jobs).
    /// When set, all default paths move under `.dev/{instance}/` and resource
    /// names are derived from the instance name instead of the local username.
    #[arg(long, env = "BL_DEV_INSTANCE")]
    instance: Option<String>,

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

    /// Server name to create/reuse
    #[arg(long, default_value_t = default_server_name())]
    server_name: String,

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

    /// Managed private key path used for provisioning and SSH
    #[arg(
        long,
        env = "BL_DEV_SSH_KEY_PATH",
        default_value = DEFAULT_MANAGED_SSH_KEY_PATH
    )]
    ssh_key_path: PathBuf,

    /// Managed account SSH key name in BinaryLane
    #[arg(long, env = "BL_DEV_SSH_KEY_NAME", default_value_t = default_ssh_key_name())]
    ssh_key_name: String,

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
    /// Instance name for parallel dev environments (e.g. CI matrix jobs).
    /// When set, all default paths move under `.dev/{instance}/` and resource
    /// names are derived from the instance name instead of the local username.
    #[arg(long, env = "BL_DEV_INSTANCE")]
    instance: Option<String>,

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
    networks: Networks,
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

/// Replace a path field with an instance-scoped value when it still holds its
/// compile-time default (i.e. the user did not explicitly override it).
fn override_path_default(field: &mut PathBuf, default: &str, instance_path: &str) {
    if *field == Path::new(default) {
        *field = PathBuf::from(instance_path);
    }
}

/// Apply instance-specific default overrides to the path fields shared by both
/// `DevUpArgs` and `DevDownArgs`.
fn apply_common_instance_paths(
    inst: &str,
    state_file: &mut PathBuf,
    kubeconfig_out: &mut PathBuf,
    known_hosts: &mut PathBuf,
    docker_config_dir: &mut PathBuf,
    tilt_values_out: &mut PathBuf,
) {
    override_path_default(
        state_file,
        DEFAULT_STATE_FILE,
        &format!(".dev/{inst}/state.json"),
    );
    override_path_default(
        kubeconfig_out,
        DEFAULT_KUBECONFIG_OUT,
        &format!(".dev/{inst}/kubeconfig"),
    );
    override_path_default(
        known_hosts,
        DEFAULT_KNOWN_HOSTS,
        &format!(".dev/{inst}/known_hosts"),
    );
    override_path_default(
        docker_config_dir,
        DEFAULT_DOCKER_CONFIG_DIR,
        &format!(".dev/{inst}/docker"),
    );
    override_path_default(
        tilt_values_out,
        DEFAULT_TILT_VALUES_OUT,
        &format!(".dev/{inst}/tilt-values.yaml"),
    );
}

/// Apply instance-specific default overrides to `DevUpArgs`.
///
/// For each field, we only overwrite it if the user did not explicitly pass a
/// value (detected by comparing against the compile-time / computed default).
fn apply_instance_overrides_up(args: &mut DevUpArgs) {
    let inst = match &args.instance {
        Some(i) => i.as_str(),
        None => return,
    };

    apply_common_instance_paths(
        inst,
        &mut args.state_file,
        &mut args.kubeconfig_out,
        &mut args.known_hosts,
        &mut args.docker_config_dir,
        &mut args.tilt_values_out,
    );
    override_path_default(
        &mut args.ssh_key_path,
        DEFAULT_MANAGED_SSH_KEY_PATH,
        &format!(".dev/{inst}/ssh-key"),
    );
    if args.server_name == default_server_name() {
        args.server_name = format!("blc-{inst}");
    }
    if args.ssh_key_name == default_ssh_key_name() {
        args.ssh_key_name = format!("blc-{inst}");
    }
}

/// Apply instance-specific default overrides to `DevDownArgs`.
fn apply_instance_overrides_down(args: &mut DevDownArgs) {
    let inst = match &args.instance {
        Some(i) => i.as_str(),
        None => return,
    };

    apply_common_instance_paths(
        inst,
        &mut args.state_file,
        &mut args.kubeconfig_out,
        &mut args.known_hosts,
        &mut args.docker_config_dir,
        &mut args.tilt_values_out,
    );
}

fn cmd_dev_up(mut args: DevUpArgs) -> Result<()> {
    let t_total = Instant::now();
    apply_instance_overrides_up(&mut args);
    ensure_tool("ssh", "install OpenSSH client")?;
    ensure_tool("ssh-keygen", "install OpenSSH tools")?;

    ensure_parent_dir(&args.state_file)?;
    ensure_parent_dir(&args.kubeconfig_out)?;
    ensure_parent_dir(&args.known_hosts)?;
    ensure_parent_dir(&args.ssh_key_path)?;
    ensure_parent_dir(&args.tilt_values_out)?;

    let timeout = Duration::from_secs(args.wait_timeout_secs);
    let ssh_key_path = absolute_path(&args.ssh_key_path)?;
    let docker_config_dir = absolute_path(&args.docker_config_dir)?;
    let tilt_values_out = absolute_path(&args.tilt_values_out)?;
    ensure_dir(&docker_config_dir)?;
    ensure_managed_ssh_keypair(&ssh_key_path)?;
    let ssh_public_key = read_managed_public_key(&ssh_key_path)?;

    let mut state = load_state(&args.state_file)?;
    if state.server_name.is_empty() {
        state.server_name = args.server_name.clone();
    }

    let client = BlClient::new(args.bl_api_token.clone())?;
    let account_key = ensure_account_ssh_key(
        &client,
        &args.ssh_key_name,
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
                create_dev_server(&client, &args, &account_key.fingerprint)?
            }
        }
    } else {
        create_dev_server(&client, &args, &account_key.fingerprint)?
    };

    state.server_id = Some(server.id);
    state.server_name = server.name.clone();
    state.region = args.region.clone();
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
    // Cost 4 (minimum) is fine for a dev-only registry credential.
    let registry_password_bcrypt =
        hash(&registry_password, 4).context("hashing registry password for basic auth")?;
    let registry_htpasswd = format!("{}:{}\n", registry_username, registry_password_bcrypt);

    state.registry_host = Some(registry_host.clone());
    state.registry_username = registry_username.clone();
    state.registry_password = Some(registry_password.clone());
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
    )?;
    eprintln!(
        "  k3s ready ............... {:.1}s",
        t.elapsed().as_secs_f64()
    );

    let registry_config = RegistryConfig {
        host: &registry_host,
        username: &registry_username,
        password: &registry_password,
        htpasswd: &registry_htpasswd,
        secret_name: &args.registry_secret_name,
    };

    let t = Instant::now();
    ensure_dev_registry(
        &server_ip,
        &ssh_user,
        Some(&ssh_key_path),
        &args.known_hosts,
        &registry_config,
        timeout,
    )?;
    eprintln!(
        "  registry deployed ....... {:.1}s",
        t.elapsed().as_secs_f64()
    );

    let t = Instant::now();
    wait_for_registry(
        &registry_host,
        &registry_username,
        &registry_password,
        timeout,
    )?;
    eprintln!(
        "  registry http ready ..... {:.1}s",
        t.elapsed().as_secs_f64()
    );

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
    let kubeconfig = rewrite_kubeconfig_server(&raw_kubeconfig, &k3s_url);
    let registry_image_repo = format!("{}/binarylane-controller", registry_host);

    fs::write(&args.kubeconfig_out, kubeconfig).with_context(|| {
        format!(
            "writing local kubeconfig to {}",
            args.kubeconfig_out.display()
        )
    })?;
    write_docker_config(
        &docker_config_dir,
        &registry_host,
        &registry_username,
        &registry_password,
    )?;
    write_dev_tilt_values(
        &tilt_values_out,
        &args,
        &account_key.fingerprint,
        &k3s_url,
        &k3s_token,
    )?;

    state.server_id = Some(server.id);
    state.server_name = server.name.clone();
    state.server_ip = Some(server_ip.clone());
    state.region = args.region.clone();
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

fn cmd_dev_down(mut args: DevDownArgs) -> Result<()> {
    apply_instance_overrides_down(&mut args);
    let state = load_state(&args.state_file)?;

    let needs_api = state.server_id.is_some() || state.ssh_key_id.is_some();
    let client = if needs_api {
        let token = args.bl_api_token.ok_or_else(|| {
            anyhow::anyhow!(
                "BL_API_TOKEN is required to delete tracked server/key resources (set env var or pass --bl-api-token)"
            )
        })?;
        Some(BlClient::new(token)?)
    } else {
        None
    };

    if let Some(server_id) = state.server_id {
        eprintln!("Deleting tracked server id={}...", server_id);
        client
            .as_ref()
            .context("internal error: API client missing")?
            .delete_server(server_id)?;
        eprintln!("Deleted tracked server id={}", server_id);
    } else {
        eprintln!("No tracked server id in state; skipping remote delete");
    }

    if let Some(ssh_key_id) = state.ssh_key_id {
        eprintln!("Deleting tracked account SSH key id={}...", ssh_key_id);
        client
            .as_ref()
            .context("internal error: API client missing")?
            .delete_account_ssh_key(ssh_key_id)?;
        eprintln!("Deleted tracked account SSH key id={}", ssh_key_id);
    } else {
        eprintln!("No tracked account SSH key id in state; skipping key delete");
    }

    remove_file_if_exists(&args.kubeconfig_out)?;
    remove_file_if_exists(&args.known_hosts)?;
    remove_file_if_exists(&args.tilt_values_out)?;

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
    ssh_key_fingerprint: &str,
) -> Result<Server> {
    eprintln!(
        "Creating dev control plane server '{}' (region={}, size={}, image={}, ssh_key={})",
        args.server_name, args.region, args.size, args.image, args.ssh_key_name
    );

    let server_password = generate_server_password();

    let server = client
        .create_server(CreateServerRequest {
            name: args.server_name.clone(),
            size: args.size.clone(),
            image: args.image.clone(),
            region: args.region.clone(),
            user_data: None,
            ssh_keys: Some(vec![ssh_key_fingerprint.to_string()]),
            password: Some(server_password),
        })
        .with_context(|| {
            "creating BinaryLane server (override defaults with --region/--size/--image if needed)"
        })?;

    eprintln!("Created server id={} name={}", server.id, server.name);
    Ok(server)
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
        for user in users {
            if ssh_probe(host, user, ssh_key_path, known_hosts)? {
                eprintln!("SSH is reachable on {} as {}", host, user);
                return Ok(user.clone());
            }
        }

        if attempts == 1 || attempts.is_multiple_of(6) {
            eprintln!(
                "Still waiting for SSH on {} (tried users: {})",
                host,
                users.join(", ")
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
) -> Result<()> {
    eprintln!("Installing/verifying k3s control plane on {}...", host);
    let start = Instant::now();
    let k3s_url = format!("https://{}:6443", host);
    let registries_yaml = format!(
        "mirrors:\n  \"{registry_host}\":\n    endpoint:\n      - \"http://{registry_host}\"\nconfigs:\n  \"{registry_host}\":\n    auth:\n      username: {registry_username}\n      password: {registry_password}\n",
        registry_host = registry.host,
        registry_username = registry.username,
        registry_password = registry.password,
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
cat <<'EOF_REGISTRIES' | ${{SUDO}} tee /etc/rancher/k3s/registries.yaml >/dev/null\n\
{registries_yaml}\
EOF_REGISTRIES\n\
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
  curl -sfL https://get.k3s.io | ${{SUDO}} env INSTALL_K3S_EXEC='server --write-kubeconfig-mode 644 --disable traefik --tls-san {host}' sh -s -\n\
elif [ \"$k3s_was_active\" -eq 1 ]; then\n\
  ${{SUDO}} systemctl restart k3s\n\
fi\n\
${{SUDO}} systemctl enable --now k3s >/dev/null 2>&1 || true\n\
${{SUDO}} systemctl is-active --quiet k3s\n",
        registries_yaml = registries_yaml,
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

struct RegistryConfig<'a> {
    host: &'a str,
    username: &'a str,
    password: &'a str,
    htpasswd: &'a str,
    secret_name: &'a str,
}

struct K3sRegistryConfig<'a> {
    host: &'a str,
    username: &'a str,
    password: &'a str,
}

fn ensure_dev_registry(
    host: &str,
    user: &str,
    ssh_key_path: Option<&Path>,
    known_hosts: &Path,
    registry: &RegistryConfig<'_>,
    timeout: Duration,
) -> Result<()> {
    eprintln!(
        "Deploying authenticated dev registry at http://{}...",
        registry.host
    );

    let manifest = format!(
        r#"apiVersion: v1
kind: Namespace
metadata:
  name: {ns}
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: registry
  namespace: {ns}
spec:
  replicas: 1
  selector:
    matchLabels:
      app: registry
  template:
    metadata:
      labels:
        app: registry
    spec:
      containers:
        - name: registry
          image: registry:2
          imagePullPolicy: IfNotPresent
          env:
            - name: REGISTRY_AUTH
              value: htpasswd
            - name: REGISTRY_AUTH_HTPASSWD_REALM
              value: dev
            - name: REGISTRY_AUTH_HTPASSWD_PATH
              value: /auth/htpasswd
          ports:
            - containerPort: 5000
              name: http
          volumeMounts:
            - name: data
              mountPath: /var/lib/registry
            - name: auth
              mountPath: /auth/htpasswd
              subPath: htpasswd
          securityContext:
            runAsUser: 0
            runAsGroup: 0
      volumes:
        - name: data
          hostPath:
            path: {registry_data_hostpath}
            type: DirectoryOrCreate
        - name: auth
          secret:
            secretName: registry-auth
---
apiVersion: v1
kind: Secret
metadata:
  name: registry-auth
  namespace: {ns}
type: Opaque
stringData:
  htpasswd: |
{registry_htpasswd}
---
apiVersion: v1
kind: Service
metadata:
  name: registry
  namespace: {ns}
spec:
  type: NodePort
  selector:
    app: registry
  ports:
    - name: http
      port: 5000
      targetPort: 5000
      nodePort: 30500
"#,
        ns = REGISTRY_NAMESPACE,
        registry_data_hostpath = REGISTRY_DATA_HOSTPATH,
        registry_htpasswd = indent_block(registry.htpasswd, 4),
    );

    let default_sa_patch = format!(
        r#"{{"imagePullSecrets":[{{"name":"{}"}}]}}"#,
        registry.secret_name
    );

    let script = format!(
        r#"set -eu
if [ "$(id -u)" -eq 0 ]; then
  SUDO=""
else
  SUDO="sudo"
fi
cat <<'EOF_MANIFEST' | ${{SUDO}} kubectl apply -f -
{manifest}
EOF_MANIFEST
for ns in {registry_ns} default binarylane-system; do
  ${{SUDO}} kubectl get namespace "$ns" >/dev/null 2>&1 || ${{SUDO}} kubectl create namespace "$ns"
  ${{SUDO}} kubectl -n "$ns" create secret docker-registry {registry_secret_name} \
    --docker-server={registry_host} \
    --docker-username={registry_username} \
    --docker-password={registry_password} \
    --dry-run=client -o yaml | ${{SUDO}} kubectl apply -f -
done
for ns in default; do
  ${{SUDO}} kubectl -n "$ns" patch serviceaccount default --type='merge' -p={default_sa_patch} >/dev/null
done
${{SUDO}} kubectl -n {registry_ns} rollout status deployment/registry --timeout=300s
"#,
        manifest = manifest,
        registry_ns = REGISTRY_NAMESPACE,
        registry_secret_name = shell_single_quote(registry.secret_name),
        registry_host = shell_single_quote(registry.host),
        registry_username = shell_single_quote(registry.username),
        registry_password = shell_single_quote(registry.password),
        default_sa_patch = shell_single_quote(&default_sa_patch),
    );

    let start = Instant::now();
    loop {
        match run_ssh_script(
            host,
            user,
            ssh_key_path,
            known_hosts,
            &script,
            SshOutputMode::Forward,
        ) {
            Ok(_) => {
                eprintln!("Dev registry is configured");
                return Ok(());
            }
            Err(err) => {
                if start.elapsed() > timeout {
                    return Err(err).context("timed out deploying dev registry");
                }
                eprintln!("dev registry not ready yet: {err}");
                thread::sleep(Duration::from_secs(10));
            }
        }
    }
}

fn wait_for_registry(
    registry_host: &str,
    registry_username: &str,
    registry_password: &str,
    timeout: Duration,
) -> Result<()> {
    eprintln!(
        "Waiting for registry endpoint http://{}/v2/ ...",
        registry_host
    );

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("building HTTP client for registry probe")?;

    let start = Instant::now();
    loop {
        let resp = client
            .get(format!("http://{registry_host}/v2/"))
            .basic_auth(registry_username, Some(registry_password))
            .send();

        match resp {
            Ok(resp) if resp.status().is_success() => {
                eprintln!(
                    "Registry endpoint is reachable ({:.0}s)",
                    start.elapsed().as_secs_f64()
                );
                return Ok(());
            }
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

        if start.elapsed() > timeout {
            bail!(
                "timed out waiting for registry endpoint http://{}/v2/",
                registry_host
            );
        }

        thread::sleep(Duration::from_secs(5));
    }
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
) -> Result<bool> {
    let output = ssh_base_command(host, user, ssh_key_path, known_hosts)
        .arg("true")
        .output()
        .context("probing SSH connectivity")?;
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
    ]);

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

fn write_dev_tilt_values(
    tilt_values_path: &Path,
    args: &DevUpArgs,
    ssh_key_fingerprint: &str,
    k3s_url: &str,
    k3s_token: &str,
) -> Result<()> {
    ensure_parent_dir(tilt_values_path)?;

    let contents = format!(
        r#"autoscaler:
  enabled: true
  listenAddr: "0.0.0.0:8086"
  config:
    namePrefix: "bl-dev-"
    sshKeys:
      - "{ssh_key_fingerprint}"
    nodeGroups:
      - id: "{group_id}"
        minSize: 0
        maxSize: 3
        size: "{size}"
        region: "{region}"
        image: "{image}"
        vcpus: 1
        memoryMb: 1024
        diskGb: 25
        labels:
          autoscale-group: "{group_id}"
mtls:
  enabled: true
templateVars:
  K3S_URL: "{k3s_url}"
  K3S_TOKEN: "{k3s_token}"
"#,
        ssh_key_fingerprint = yaml_escape(ssh_key_fingerprint),
        group_id = DEV_AUTOSCALER_GROUP_ID,
        size = yaml_escape(&args.size),
        region = yaml_escape(&args.region),
        image = yaml_escape(&args.image),
        k3s_url = yaml_escape(k3s_url),
        k3s_token = yaml_escape(k3s_token),
    );

    fs::write(tilt_values_path, contents).with_context(|| {
        format!(
            "writing generated tilt values {}",
            tilt_values_path.display()
        )
    })
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

fn default_server_name() -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "dev".to_string());
    let cleaned = sanitize_name_component(&user);
    format!("binarylane-dev-cp-{cleaned}")
}

fn default_ssh_key_name() -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "dev".to_string());
    let cleaned = sanitize_name_component(&user);
    format!("binarylane-dev-key-{cleaned}")
}

fn sanitize_name_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "dev".to_string()
    } else {
        out
    }
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
