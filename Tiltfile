load("ext://helm_resource", "helm_resource", "helm_repo")

allow_k8s_contexts("default")

registry_image = os.getenv("BL_DEV_CONTROLLER_IMAGE", "ghcr.io/samcday/binarylane-controller")
registry_pull_secret = os.getenv("BL_DEV_REGISTRY_PULL_SECRET", "dev-registry-cred")
binarylane_api_token = os.getenv("BL_API_TOKEN", "")
generated_values = os.getenv("BL_DEV_TILT_VALUES", ".dev/tilt-values.generated.yaml")

if binarylane_api_token == "":
    fail(
        "BL_API_TOKEN is empty. Run: eval \"$(BL_API_TOKEN=$(cat ~/.bltoken) cargo xtask dev-up)\" and then retry tilt up."
    )

if not os.path.exists(generated_values):
    fail(
        "Generated dev values file is missing at " + generated_values + ". Run: eval \"$(BL_API_TOKEN=$(cat ~/.bltoken) cargo xtask dev-up)\""
    )

if registry_image == "ghcr.io/samcday/binarylane-controller":
    print(
        "warning: BL_DEV_CONTROLLER_IMAGE is not set; using GHCR fallback. Run eval \"$(BL_API_TOKEN=$(cat ~/.bltoken) cargo xtask dev-up)\" for remote dev registry wiring."
    )

# No live_update: the distroless runtime image has no tar/rm, which Tilt
# needs for file sync. Full rebuild via the dep-cached Dockerfile is ~30s.
docker_build(
    registry_image,
    ".",
    dockerfile="Dockerfile",
    only=["Cargo.toml", "Cargo.lock", "build.rs", "src/", "proto/", "xtask/Cargo.toml"],
)

# Deploy via Helm chart
k8s_yaml(helm(
    "chart",
    name="binarylane-controller",
    namespace="binarylane-system",
    set=[
        "apiToken=" + binarylane_api_token,
        "image.repository=" + registry_image,
        "image.tag=latest",
        "image.pullPolicy=Always",
        "imagePullSecrets[0].name=" + registry_pull_secret,
    ],
    values=["tilt-values.yaml", generated_values],
))

k8s_resource(
    "binarylane-controller",
    port_forwards="8086:8086",
    labels=["controller"],
)

# Official cluster-autoscaler chart for dev autoscaler testing
helm_repo("autoscaler-charts", "https://kubernetes.github.io/autoscaler")

k8s_yaml(blob("""
apiVersion: v1
kind: ConfigMap
metadata:
  name: cluster-autoscaler-cloud-config
  namespace: binarylane-system
data:
  cloud-config: |
    address: "binarylane-controller.binarylane-system.svc:8086"
    key: "/etc/ssl/client-cert/tls.key"
    cert: "/etc/ssl/client-cert/tls.crt"
    cacert: "/etc/ssl/client-cert/ca.crt"
"""))

helm_resource(
    "cluster-autoscaler",
    "autoscaler-charts/cluster-autoscaler",
    namespace="binarylane-system",
    flags=["--values=cluster-autoscaler-dev-values.yaml"],
    resource_deps=["binarylane-controller"],
    labels=["autoscaler"],
)
