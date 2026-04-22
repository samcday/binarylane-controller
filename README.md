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
| `NAMESPACE` | no | `binarylane-system` | Target-cluster namespace for per-node Secrets |
| `GRPC_LISTEN_ADDR` | no | `:8086` | gRPC listen address |

## AutoScalingGroup templating

An `AutoScalingGroup` declares the shape of Nodes to be provisioned. Two
templating layers are available:

**`spec.userData`** is a [MiniJinja](https://docs.rs/minijinja) cloud-init
template, rendered per-node at scale-up time. Built-in variables:

- `node.name`, `node.hostname`, `node.index`, `node.password`
- `asg.name`, `asg.size`, `asg.region`, `asg.image`, `asg.namePrefix`
- `asg.vcpus`, `asg.memoryMb`, `asg.diskGb`

Additional variables go under `spec.templateVariables`; each variable is
either a literal value or a reference to a Secret / ConfigMap key. Values
are resolved once per scale-up RPC and shared across every Node it creates.
Because cloud-init only runs at first boot, changing a referenced Secret
after a Node is provisioned does not re-render the Node's user-data.

**`spec.template`** sets labels, annotations, and taints on every Node
created by the ASG — mirroring `Deployment.spec.template`. Controller-owned
keys (for example `kubernetes.io/hostname`, the uninitialized taint) take
precedence; colliding user values are dropped and surfaced as a Warning
Event on the ASG.

```yaml
apiVersion: blc.samcday.com/v1alpha1
kind: AutoScalingGroup
metadata:
  name: gpu-workers
spec:
  minSize: 0
  maxSize: 3
  size: gpu-1
  region: syd
  image: ubuntu-22.04
  userData: |
    #cloud-config
    runcmd:
      - kubeadm join cp.internal:6443 \
          --token {{ joinToken }} \
          --node-name {{ node.name }}
  templateVariables:
    - name: joinToken
      valueFrom:
        secretKeyRef:
          name: kubeadm-join-token
          key: token
  template:
    metadata:
      labels:
        role: gpu-worker
    spec:
      taints:
        - key: gpu
          value: "true"
          effect: NoSchedule
```

## Remote dev control plane

Use xtask to provision/reuse a BinaryLane k3s control-plane VM and emit sourceable env vars:

```bash
eval "$(BL_API_TOKEN=<your-token> cargo xtask dev-up)"
```

`dev-up` writes local state to `.dev/dev-state.json` and kubeconfig to `.dev/kubeconfig`, logs progress to stderr, and prints `export KEY='value'` shell assignments to stdout (including `KUBECONFIG`, `DOCKER_CONFIG`, `BL_DEV_*`, and `TMPL_*` values).

It also provisions an authenticated public registry endpoint on the dev control-plane host, backed by `hostPath` storage, and writes Docker auth to `.dev/docker/config.json` so local `docker`/Tilt pushes can work immediately via exported `DOCKER_CONFIG`.

`Tiltfile` consumes `BL_DEV_CONTROLLER_IMAGE` and `BL_DEV_REGISTRY_PULL_SECRET` from this environment automatically.
`BL_API_TOKEN` is also exported so `tilt up` and `cargo xtask dev-down` work without extra setup.

Tear down all tracked dev resources and local state:

```bash
BL_API_TOKEN=<your-token> cargo xtask dev-down
```
