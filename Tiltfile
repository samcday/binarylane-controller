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

# Build natively + crane append (no Dockerfile). Fast for both dev
# (incremental cargo build) and CI (cached cargo build).
custom_build(
    registry_image,
    'cargo build --release && tar cf /tmp/bl-controller-image.tar -C target/release binarylane-controller && crane append -b ubuntu:24.04 -f /tmp/bl-controller-image.tar -t $EXPECTED_REF --insecure && crane mutate $EXPECTED_REF --entrypoint /binarylane-controller --insecure',
    deps=["Cargo.toml", "Cargo.lock", "build.rs", "src/", "proto/", "binarylane-client/"],
    skips_local_docker=True,
)

# Generate stable mTLS certs for dev. These persist in .dev/ and are only
# regenerated if the files are deleted. This avoids the problem where Helm's
# genCA creates new certs on every `helm template` (client-side) re-render,
# breaking mTLS between pods that mount different generations of certs.
if not os.path.exists(".dev/mtls-ca.crt"):
    local(" && ".join([
        "openssl req -x509 -newkey rsa:2048 -keyout .dev/mtls-ca.key -out .dev/mtls-ca.crt -days 3650 -nodes -subj '/CN=binarylane-controller-ca' 2>/dev/null",
        "openssl req -newkey rsa:2048 -keyout .dev/mtls-server.key -out .dev/mtls-server.csr -nodes -subj '/CN=binarylane-controller' -addext 'subjectAltName=DNS:binarylane-controller.binarylane-system.svc,DNS:binarylane-controller.binarylane-system.svc.cluster.local,DNS:localhost' 2>/dev/null",
        "openssl x509 -req -in .dev/mtls-server.csr -CA .dev/mtls-ca.crt -CAkey .dev/mtls-ca.key -CAcreateserial -out .dev/mtls-server.crt -days 3650 -copy_extensions copyall 2>/dev/null",
        "openssl req -newkey rsa:2048 -keyout .dev/mtls-autoscaler-client.key -out .dev/mtls-autoscaler-client.csr -nodes -subj '/CN=cluster-autoscaler' 2>/dev/null",
        "openssl x509 -req -in .dev/mtls-autoscaler-client.csr -CA .dev/mtls-ca.crt -CAkey .dev/mtls-ca.key -CAcreateserial -out .dev/mtls-autoscaler-client.crt -days 3650 2>/dev/null",
        "openssl req -newkey rsa:2048 -keyout .dev/mtls-external-dns-client.key -out .dev/mtls-external-dns-client.csr -nodes -subj '/CN=external-dns' 2>/dev/null",
        "openssl x509 -req -in .dev/mtls-external-dns-client.csr -CA .dev/mtls-ca.crt -CAkey .dev/mtls-ca.key -CAcreateserial -out .dev/mtls-external-dns-client.crt -days 3650 2>/dev/null",
    ]), quiet=True)

_ca_crt = str(read_file(".dev/mtls-ca.crt")).strip()
_server_crt = str(read_file(".dev/mtls-server.crt")).strip()
_server_key = str(read_file(".dev/mtls-server.key")).strip()
_autoscaler_client_crt = str(read_file(".dev/mtls-autoscaler-client.crt")).strip()
_autoscaler_client_key = str(read_file(".dev/mtls-autoscaler-client.key")).strip()
_ed_client_crt = str(read_file(".dev/mtls-external-dns-client.crt")).strip()
_ed_client_key = str(read_file(".dev/mtls-external-dns-client.key")).strip()

# Deploy via Helm chart. mTLS is enabled so the deployment gets TLS env vars
# and volume mounts, but we override the helm-generated cert secrets below
# with our stable dev certs.
_helm_set = [
    "apiToken=" + binarylane_api_token,
    "image.repository=" + registry_image,
    "image.tag=latest",
    "image.pullPolicy=Always",
    "imagePullSecrets[0].name=" + registry_pull_secret,
    # Skip Helm cert generation — we manage mTLS secrets ourselves above.
    # mtls.enabled stays true so the deployment gets TLS env vars + volume mounts.
    "mtls.generate=false",
]
_rust_log = os.getenv("RUST_LOG", "")
if _rust_log:
    _helm_set.append("extraEnv[0].name=RUST_LOG")
    _helm_set.append("extraEnv[0].value=" + _rust_log)

crds_manifests = helm(
    "charts/binarylane-controllers-crds",
    name="binarylane-controllers-crds",
    namespace="binarylane-system",
)
k8s_yaml(crds_manifests)

controller_manifests = helm(
    "charts/binarylane-controllers",
    name="binarylane-controllers",
    namespace="binarylane-system",
    set=_helm_set,
    values=["tilt-values.yaml", generated_values],
)
k8s_yaml(controller_manifests)

# Dev ASG and user-data Secret generated by xtask
dev_resources = os.getenv("BL_DEV_RESOURCES", ".dev/dev-resources.generated.yaml")
if os.path.exists(dev_resources):
    k8s_yaml(dev_resources)

# Annotate control plane node for adoption by node-bind controller.
# In dev there's only the CP node at startup; workers are added by autoscaler.
local("kubectl annotate node --all bl.samcday.com/adopt= --overwrite 2>/dev/null || true", quiet=True)

# Override helm-generated mTLS secrets with stable dev certs. Applied after
# controller_manifests so these take precedence.
k8s_yaml(blob("""
apiVersion: v1
kind: Secret
metadata:
  name: binarylane-controller-mtls-server
  namespace: binarylane-system
type: Opaque
stringData:
  ca.crt: |
    """ + _ca_crt.replace("\n", "\n    ") + """
  tls.crt: |
    """ + _server_crt.replace("\n", "\n    ") + """
  tls.key: |
    """ + _server_key.replace("\n", "\n    ") + """
---
apiVersion: v1
kind: Secret
metadata:
  name: binarylane-controller-mtls-client
  namespace: binarylane-system
type: Opaque
stringData:
  ca.crt: |
    """ + _ca_crt.replace("\n", "\n    ") + """
  tls.crt: |
    """ + _autoscaler_client_crt.replace("\n", "\n    ") + """
  tls.key: |
    """ + _autoscaler_client_key.replace("\n", "\n    ") + """
---
apiVersion: v1
kind: Secret
metadata:
  name: binarylane-controller-external-dns-mtls-client
  namespace: binarylane-system
type: Opaque
stringData:
  ca.crt: |
    """ + _ca_crt.replace("\n", "\n    ") + """
  tls.crt: |
    """ + _ed_client_crt.replace("\n", "\n    ") + """
  tls.key: |
    """ + _ed_client_key.replace("\n", "\n    ") + """
"""))

# Stable cert hash — doesn't change across re-renders since certs are on disk.
_mtls_hash = str(hash(_ca_crt))

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

_ed_flags = ["--values=external-dns-dev-values.yaml"]
_ed_flags.append("--set-string=podAnnotations.checksum/mtls-client=" + _mtls_hash)

helm_resource(
    "external-dns",
    "external-dns-charts/external-dns",
    namespace="binarylane-system",
    flags=_ed_flags,
    resource_deps=["binarylane-controller"],
    labels=["dns"],
)

# Dev-cleanup heartbeat: keep the dev-heartbeat Secret updated while Tilt is
# running. When Tilt stops (e.g. Codespace is stopped/deleted), the heartbeat
# goes stale and the dev-cleanup controller tears down all BL resources.
local_resource(
    'heartbeat',
    serve_cmd='while true; do kubectl -n binarylane-system apply -f - <<YAML\napiVersion: v1\nkind: Secret\nmetadata:\n  name: dev-heartbeat\n  namespace: binarylane-system\n  labels:\n    blc.samcday.com/dev-heartbeat: "true"\nstringData:\n  timestamp: "$(date +%s)"\nYAML\nsleep 300; done',
    resource_deps=['binarylane-controller'],
    labels=["dev"],
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
