# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Is

A Kubernetes cloud provider controller for BinaryLane (Australian VPS provider). Three responsibilities:
1. **Node controller** - reconciles K8s nodes with BinaryLane servers (syncs addresses/labels/taints, removes nodes for deleted servers)
2. **Service controller** - manages LoadBalancer-type services by creating/updating/deleting BinaryLane load balancers
3. **Autoscaler gRPC provider** - implements the K8s cluster-autoscaler external gRPC CloudProvider interface to scale node groups

## Build & Run

```bash
cargo check --workspace --locked    # type check
cargo clippy --workspace --locked   # lint
cargo fmt --all -- --check          # format check
cargo test --workspace --locked     # test
cargo build --release               # release build
cargo xtask dev                     # local dev with kind + bacon auto-reload
cargo xtask tilt                    # in-cluster dev with Tilt + Helm
```

## Architecture

```
src/main.rs                  Entry point: CLI args, starts controllers + optional gRPC autoscaler
src/binarylane.rs            REST client for BinaryLane v2 API (servers + load balancers)
src/node_controller.rs       30s reconciliation loop over K8s nodes with binarylane:/// provider IDs
src/service_controller.rs    30s reconciliation loop for LoadBalancer services
src/autoscaler.rs            gRPC CloudProvider: manages node groups, creates/deletes servers
proto/externalgrpc.proto     Cluster autoscaler external gRPC proto definition
build.rs                     Compiles proto via tonic-build + protobuf-src
chart/                       Helm chart for deployment
xtask/                       Dev workflow automation (kind, tilt, bacon)
```

### Key concepts

- **Provider ID format**: `binarylane:///<serverID>` - maps K8s nodes to BinaryLane servers. See `binarylane::server_provider_id()` / `parse_provider_id()`.
- **Server ownership**: autoscaler identifies managed servers by name prefix (`<namePrefix><groupID>-`).
- **Cloud-init templating**: simple `{{.Key}}` string replacement with vars from `TMPL_*` env vars plus built-in `NodeName`, `NodeGroup`, `Region`, `Size`.
- **Thread safety**: autoscaler server cache protected by `tokio::sync::RwLock`.
- **Node/service controllers** run unconditionally; autoscaler gRPC server only starts if config file exists.

### Environment variables

| Variable | Required | Default | Purpose |
|---|---|---|---|
| `BL_API_TOKEN` | yes | | BinaryLane API token |
| `CONFIG_PATH` | no | `/etc/binarylane-controller/config.json` | Autoscaler node group config |
| `CLOUD_INIT_PATH` | no | `/etc/binarylane-controller/cloud-init.sh` | Cloud-init template file |
| `GRPC_LISTEN_ADDR` | no | `0.0.0.0:8086` | gRPC listen address |
| `TLS_CERT_PATH` | no | | TLS cert (enables mTLS when all three TLS vars set) |
| `TLS_KEY_PATH` | no | | TLS private key |
| `TLS_CA_PATH` | no | | TLS CA certificate |
| `TMPL_*` | no | | Template variables for cloud-init |
