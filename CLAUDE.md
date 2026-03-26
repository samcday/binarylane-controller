# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Is

A Kubernetes cloud provider controller for BinaryLane (Australian VPS provider). Two responsibilities:
1. **Node controller** - reconciles K8s nodes with BinaryLane servers (syncs addresses/labels/taints, removes nodes for deleted servers)
2. **Autoscaler gRPC provider** - implements the K8s cluster-autoscaler external gRPC CloudProvider interface to scale node groups

## Build & Run

```bash
go build -o binarylane-controller .          # build
docker build -t binarylane-controller .      # container build
buf generate                                 # regenerate proto stubs (requires protoc-gen-go, protoc-gen-go-grpc)
```

No tests exist yet. No CI/CD is configured.

## Architecture

```
main.go                        Entry point: inits BL client, starts node controller + optional gRPC autoscaler
binarylane/client.go           REST client for BinaryLane v2 API (Get/List/Create/Delete servers)
nodecontroller/controller.go   30s reconciliation loop over all K8s nodes with binarylane:/// provider IDs
autoscaler/provider.go         gRPC CloudProvider: manages node groups, creates/deletes servers, cloud-init templating
proto/                         externalgrpc.proto + generated Go stubs (do not edit .pb.go files)
```

### Key concepts

- **Provider ID format**: `binarylane:///<serverID>` - maps K8s nodes to BinaryLane servers. See `binarylane.ServerProviderID()` / `ParseProviderID()`.
- **Server ownership**: autoscaler identifies managed servers by name prefix (`<namePrefix><groupID>-`).
- **Cloud-init templating**: `text/template` with vars from `TMPL_*` env vars plus built-in `NodeName`, `NodeGroup`, `Region`, `Size`.
- **Thread safety**: autoscaler server cache protected by `sync.RWMutex`.
- **Node controller** runs unconditionally; autoscaler gRPC server only starts if config file exists.

### Environment variables

| Variable | Required | Default | Purpose |
|---|---|---|---|
| `BL_API_TOKEN` | yes | | BinaryLane API token |
| `KUBECONFIG` | no | in-cluster | Path to kubeconfig |
| `CONFIG_PATH` | no | `/etc/binarylane-controller/config.json` | Autoscaler node group config |
| `CLOUD_INIT_PATH` | no | `/etc/binarylane-controller/cloud-init.sh` | Cloud-init template file |
| `GRPC_LISTEN_ADDR` | no | `:8086` | gRPC listen address |
| `TMPL_*` | no | | Template variables for cloud-init |
