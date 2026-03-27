load("ext://helm_resource", "helm_resource", "helm_repo")

# Build the container image
docker_build(
    "ghcr.io/samcday/binarylane-controller",
    ".",
    dockerfile="Dockerfile",
    only=["Cargo.toml", "Cargo.lock", "build.rs", "src/", "proto/"],
    live_update=[
        sync("src/", "/src/src/"),
        run("cd /src && cargo build --release", trigger=["src/"]),
    ],
)

# Deploy via Helm chart
k8s_yaml(helm(
    "chart",
    name="binarylane-controller",
    namespace="binarylane-system",
    set=[
        "image.repository=ghcr.io/samcday/binarylane-controller",
        "image.tag=latest",
        "image.pullPolicy=Never",
    ],
    values=["tilt-values.yaml"],
))

k8s_resource(
    "binarylane-controller",
    port_forwards="8086:8086",
    labels=["controller"],
)
