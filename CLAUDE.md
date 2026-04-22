# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Is

A Kubernetes cloud provider controller for BinaryLane (Australian VPS provider). Individually-toggleable reconcilers managed via `--controllers` flag (kube-controller-manager style), plus optional servers enabled via separate flags:

1. **node-sync** - syncs K8s node addresses/labels/taints from BinaryLane server metadata
2. **node-deletion** - handles node/server cleanup (deletion, orphan removal, finalizer management)
3. **node-bind** - matches unbound K8s nodes to BinaryLane servers by hostname (opt-in via `bl.samcday.com/adopt` annotation)
4. **node-provision** - provisions BinaryLane servers for Nodes with provision labels (`bl.samcday.com/size`, `region`, `image`)
5. **service** - manages LoadBalancer-type services via BinaryLane load balancers
6. **autoscaler** - gRPC CloudProvider for cluster-autoscaler (enabled via `--cluster-autoscaler-service`)
7. **dns-webhook** - external-dns webhook provider (enabled via `--external-dns-webhook`)

## Build & Run

```bash
cargo check --workspace --locked    # type check
cargo clippy --workspace --locked   # lint
cargo fmt --all -- --check          # format check
cargo test --workspace --locked     # test
cargo build --release               # release build
cargo xtask dev-up                  # provision/reuse remote BinaryLane k3s control plane
cargo xtask dev-down                # tear down remote BinaryLane dev control plane
```

## Architecture

```
src/main.rs                      Entry point: CLI args, controller selection, conditional spawning
src/controllers/
  mod.rs                         Shared types (ReconcileContext), constants, resolve_controllers()
  node_sync.rs                   Address/label/taint sync from BL server
  node_deletion.rs               Deletion handling, finalizer management, secret cleanup
  node_bind.rs                   Hostname-based binding of unbound nodes to BL servers
  node_provision.rs              BL server provisioning from Node labels + Secrets
  service.rs                     LoadBalancer service reconciliation
src/autoscaler.rs                gRPC CloudProvider: creates Node + Secrets for node-provision
src/dns_webhook.rs               External-DNS webhook server
proto/externalgrpc.proto         Cluster autoscaler external gRPC proto definition
build.rs                         Compiles proto via tonic-build
charts/                          Helm charts: binarylane-controllers (controller) + binarylane-controllers-crds (CRDs)
xtask/                           Dev workflow automation (remote k3s dev control plane)
binarylane-client/               REST client crate for BinaryLane v2 API
```

### Key concepts

- **Provider ID format**: `binarylane:///<serverID>` - maps K8s nodes to BinaryLane servers. See `binarylane_client::server_provider_id()` / `parse_provider_id()`.
- **Controller selection**: `--controllers='*'` (default), `--controllers='*,-service'`, `--controllers='node-sync,node-bind'`. Parsed by `controllers::resolve_controllers()`.
- **Node-driven provisioning**: create a Node with labels `bl.samcday.com/{size,region,image}` + Secrets for password and user-data, and `node-provision` creates the BL server. The autoscaler gRPC is a thin wrapper around this.
- **Node binding**: annotate a node with `bl.samcday.com/adopt` and `node-bind` will look up the BL server by hostname (`GET /v2/servers?hostname=`).
- **Labels**: `bl.samcday.com/server-id` (queryable, set after bind/provision), `bl.samcday.com/size`, `bl.samcday.com/region`, `bl.samcday.com/image`.
- **Associated resources**: Secret `{name}-node-password`, Secret `{name}-user-data`.
- **Server ownership**: autoscaler identifies managed servers by name prefix (`<namePrefix><groupID>-`).
- **Cloud-init templating**: `spec.userData` on an `AutoScalingGroup` is a MiniJinja template, rendered per-node at scale-up. Built-in scopes: `node.{name,hostname,index,password}` and `asg.{name,size,region,image,namePrefix,vcpus,memoryMb,diskGb}`. Extra variables come from `spec.templateVariables` as literals or Secret/ConfigMap refs. Strict-undefined: typos in `{{ asg.naem }}` fail scale-up.
- **Node template**: `spec.template` mirrors `Deployment.spec.template` — `metadata.{labels,annotations}` and `spec.taints` are merged onto every provisioned Node. Reserved keys (hostname, arch, os, the uninitialized taint, etc.) are controller-owned and drop user overrides with a Warning Event. Values flow through both real Node creation and the cluster-autoscaler `NodeGroupTemplateNodeInfo` simulator so scheduling simulation stays honest.

### Environment variables

| Variable | Required | Default | Purpose |
|---|---|---|---|
| `BL_API_TOKEN` | yes | | BinaryLane API token |
| `CONTROLLERS` | no | `*` | Controller selection (comma-separated) |
| `NAMESPACE` | no | `binarylane-system` | Target-cluster namespace for per-node Secrets |
| `GRPC_LISTEN_ADDR` | no | `0.0.0.0:8086` | gRPC listen address |
| `TLS_CERT_PATH` | no | | TLS cert (enables mTLS when all three TLS vars set) |
| `TLS_KEY_PATH` | no | | TLS private key |
| `TLS_CA_PATH` | no | | TLS CA certificate |
