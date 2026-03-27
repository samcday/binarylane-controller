# binarylane-controller

Kubernetes cloud provider controller for [BinaryLane](https://www.binarylane.com.au/).

**Node controller** — reconciles K8s nodes with BinaryLane servers. Syncs addresses, labels, and taints; removes nodes whose servers no longer exist. Runs on a 30s loop.

**Autoscaler provider** — implements the cluster-autoscaler [external gRPC CloudProvider](https://github.com/kubernetes/autoscaler/tree/master/cluster-autoscaler/cloudprovider/externalgrpc) interface. Manages node groups, creates/deletes BinaryLane servers with cloud-init templating. Only starts if a config file is present.

## Quick start

```bash
export BL_API_TOKEN=<your-token>
go build -o binarylane-controller .
./binarylane-controller
```

Or with Docker:

```bash
docker build -t binarylane-controller .
docker run -e BL_API_TOKEN=<your-token> binarylane-controller
```

## Configuration

| Variable | Required | Default | Purpose |
|---|---|---|---|
| `BL_API_TOKEN` | yes | | BinaryLane API token |
| `KUBECONFIG` | no | in-cluster | Path to kubeconfig |
| `CONFIG_PATH` | no | `/etc/binarylane-controller/config.json` | Autoscaler node group config |
| `CLOUD_INIT_PATH` | no | `/etc/binarylane-controller/cloud-init.sh` | Cloud-init template file |
| `GRPC_LISTEN_ADDR` | no | `:8086` | gRPC listen address |
| `TMPL_*` | no | | Extra variables passed into the cloud-init template |

## Remote dev control plane

Use xtask to provision/reuse a BinaryLane k3s control-plane VM and emit sourceable env vars:

```bash
eval "$(cargo xtask dev-up)"
```

`dev-up` writes local state to `.dev/dev-state.json` and kubeconfig to `.dev/kubeconfig`, logs progress to stderr, and prints shell assignments to stdout (including `KUBECONFIG`, `BL_DEV_*`, and `TMPL_*` values).

Tear down all tracked dev resources and local state:

```bash
cargo xtask dev-down
```
