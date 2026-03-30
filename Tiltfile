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
    only=["Cargo.toml", "Cargo.lock", "build.rs", "src/", "proto/", "binarylane-client/", "xtask/Cargo.toml"],
)

# Deploy via Helm chart.
# Cloud-init is read from a file so edits take effect immediately via Tilt
# file-watching, without needing to re-run `cargo xtask dev-up`.
_cloud_init = str(read_file("dev-cloud-init.sh")).strip()
controller_manifests = helm(
    "chart",
    name="binarylane-controller",
    namespace="binarylane-system",
    set=[
        "apiToken=" + binarylane_api_token,
        "image.repository=" + registry_image,
        "image.tag=latest",
        "image.pullPolicy=Always",
        "imagePullSecrets[0].name=" + registry_pull_secret,
        "autoscaler.cloudInit=" + _cloud_init,
    ],
    values=["tilt-values.yaml", generated_values],
)
k8s_yaml(controller_manifests)

# Extract a change-detection hash from the mTLS client secret so we can
# roll the cluster-autoscaler pod whenever certs are regenerated.
_mtls_hash = ""
for _obj in decode_yaml_stream(controller_manifests):
    if _obj.get("kind") == "Secret" and _obj.get("metadata", {}).get("name") == "binarylane-controller-mtls-client":
        _mtls_hash = str(hash(str(_obj.get("data", {}))))
        break

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

_ca_flags = ["--values=cluster-autoscaler-dev-values.yaml"]
if _mtls_hash:
    _ca_flags.append("--set-string=podAnnotations.checksum/mtls-client=" + _mtls_hash)

helm_resource(
    "cluster-autoscaler",
    "autoscaler-charts/cluster-autoscaler",
    namespace="binarylane-system",
    flags=_ca_flags,
    resource_deps=["binarylane-controller"],
    labels=["autoscaler"],
)

# DNSEndpoint CRD for external-dns CRD source
local("KUBECONFIG=$KUBECONFIG kubectl apply -f https://raw.githubusercontent.com/kubernetes-sigs/external-dns/master/config/crd/standard/dnsendpoints.externaldns.k8s.io.yaml 2>&1 || true", quiet=True)

# Official external-dns chart for dev DNS management testing
helm_repo("external-dns-charts", "https://kubernetes-sigs.github.io/external-dns")

# Extract mTLS hash from external-dns client secret for rollout detection
_ed_mtls_hash = ""
for _obj in decode_yaml_stream(controller_manifests):
    if _obj.get("kind") == "Secret" and _obj.get("metadata", {}).get("name") == "binarylane-controller-external-dns-mtls-client":
        _ed_mtls_hash = str(hash(str(_obj.get("data", {}))))
        break

_ed_flags = ["--values=external-dns-dev-values.yaml"]
if _ed_mtls_hash:
    _ed_flags.append("--set-string=podAnnotations.checksum/mtls-client=" + _ed_mtls_hash)

helm_resource(
    "external-dns",
    "external-dns-charts/external-dns",
    namespace="binarylane-system",
    flags=_ed_flags,
    resource_deps=["binarylane-controller"],
    labels=["dns"],
)

# Scale-test deployment for exercising the autoscaler. Created with replicas=0
# so it doesn't trigger scale-up by default. Use kubectl to scale:
#   kubectl scale deploy/scale-test --replicas=3   # force node scale-up
#   kubectl scale deploy/scale-test --replicas=0   # allow scale-down
# Note: Tiltfile re-eval will reset replicas to 0. Just re-scale after.
k8s_yaml("scale-test.yaml")
k8s_resource(
    "scale-test",
    resource_deps=["cluster-autoscaler"],
    labels=["test"],
)
