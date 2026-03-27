use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "xtask", about = "Development tasks for binarylane-controller")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the controller locally with auto-reload on code changes (via bacon)
    Dev {
        /// kind cluster name to use (creates if missing)
        #[arg(long, default_value = "bl-dev")]
        cluster: String,
    },
    /// Create a kind cluster for development
    KindUp {
        /// Cluster name
        #[arg(long, default_value = "bl-dev")]
        cluster: String,
    },
    /// Delete the development kind cluster
    KindDown {
        /// Cluster name
        #[arg(long, default_value = "bl-dev")]
        cluster: String,
    },
    /// Run Tilt for in-cluster development (builds image, deploys Helm chart, watches for changes)
    Tilt {
        /// kind cluster name to use (creates if missing)
        #[arg(long, default_value = "bl-dev")]
        cluster: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Dev { cluster } => cmd_dev(&cluster),
        Commands::KindUp { cluster } => cmd_kind_up(&cluster),
        Commands::KindDown { cluster } => cmd_kind_down(&cluster),
        Commands::Tilt { cluster } => cmd_tilt(&cluster),
    }
}

fn cmd_dev(cluster: &str) -> Result<()> {
    ensure_tool("kind")?;
    ensure_tool("bacon")?;

    // Ensure kind cluster exists
    if !kind_cluster_exists(cluster)? {
        println!("Creating kind cluster '{cluster}'...");
        cmd_kind_up(cluster)?;
    } else {
        println!("Using existing kind cluster '{cluster}'");
    }

    let kubeconfig = get_kind_kubeconfig(cluster)?;
    println!("KUBECONFIG={kubeconfig}");
    println!("Starting bacon (auto-rebuild + run)...");
    println!("Set BL_API_TOKEN in your environment before running.");
    println!();

    let status = Command::new("bacon")
        .arg("run")
        .arg("--")
        .arg("--")
        .arg("--grpc-listen-addr=0.0.0.0:8086")
        .env("KUBECONFIG", &kubeconfig)
        .env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or("info".into()),
        )
        .status()
        .context("running bacon")?;

    if !status.success() {
        bail!("bacon exited with {status}");
    }
    Ok(())
}

fn cmd_kind_up(cluster: &str) -> Result<()> {
    ensure_tool("kind")?;

    let status = Command::new("kind")
        .args(["create", "cluster", "--name", cluster])
        .status()
        .context("creating kind cluster")?;
    if !status.success() {
        bail!("kind create cluster failed");
    }
    println!("Kind cluster '{cluster}' is ready.");
    println!("Kubeconfig: {}", get_kind_kubeconfig(cluster)?);
    Ok(())
}

fn cmd_kind_down(cluster: &str) -> Result<()> {
    ensure_tool("kind")?;

    let status = Command::new("kind")
        .args(["delete", "cluster", "--name", cluster])
        .status()
        .context("deleting kind cluster")?;
    if !status.success() {
        bail!("kind delete cluster failed");
    }
    println!("Kind cluster '{cluster}' deleted.");
    Ok(())
}

fn cmd_tilt(cluster: &str) -> Result<()> {
    ensure_tool("kind")?;
    ensure_tool("tilt")?;

    // Ensure kind cluster exists
    if !kind_cluster_exists(cluster)? {
        println!("Creating kind cluster '{cluster}'...");
        cmd_kind_up(cluster)?;
    } else {
        println!("Using existing kind cluster '{cluster}'");
    }

    // Create the namespace if it doesn't exist
    let _ = Command::new("kubectl")
        .args([
            "--context",
            &format!("kind-{cluster}"),
            "create",
            "namespace",
            "binarylane-system",
        ])
        .output();

    println!("Starting Tilt...");
    println!("Edit tilt-values.yaml to set your BL_API_TOKEN.");
    println!();

    let status = Command::new("tilt")
        .args(["up", "--context", &format!("kind-{cluster}")])
        .status()
        .context("running tilt")?;

    if !status.success() {
        bail!("tilt exited with {status}");
    }
    Ok(())
}

fn kind_cluster_exists(name: &str) -> Result<bool> {
    let output = Command::new("kind")
        .args(["get", "clusters"])
        .output()
        .context("listing kind clusters")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().any(|l| l.trim() == name))
}

fn get_kind_kubeconfig(cluster: &str) -> Result<String> {
    let output = Command::new("kind")
        .args(["get", "kubeconfig-path", "--name", cluster])
        .output()
        .context("getting kind kubeconfig path")?;

    if !output.status.success() {
        // Fallback: kind stores kubeconfigs in the default location
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        return Ok(format!("{home}/.kube/config"));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn ensure_tool(name: &str) -> Result<()> {
    let found = Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !found {
        bail!(
            "'{name}' not found in PATH. Install it:\n  {}",
            match name {
                "kind" => "https://kind.sigs.k8s.io/docs/user/quick-start/#installation",
                "bacon" => "cargo install bacon",
                "tilt" => "https://docs.tilt.dev/install.html",
                _ => "check the project docs",
            }
        );
    }
    Ok(())
}
