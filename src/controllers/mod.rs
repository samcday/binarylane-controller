pub mod node_bind;
pub mod node_deletion;
pub mod node_provision;
pub mod node_sync;
pub mod service;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use binarylane_client as binarylane;
use k8s_openapi::api::core::v1::{Node, Secret};
use kube::runtime::controller::Action;
use kube::runtime::reflector::ObjectRef;

pub const FINALIZER: &str = "blc.samcday.com/server-cleanup";
pub const UNINITIALIZED_TAINT: &str = "node.cloudprovider.kubernetes.io/uninitialized";

// Labels
pub const LABEL_ASG: &str = "blc.samcday.com/asg";
pub const LABEL_SERVER_ID: &str = "bl.samcday.com/server-id";
pub const LABEL_SIZE: &str = "bl.samcday.com/size";
pub const LABEL_REGION: &str = "bl.samcday.com/region";
pub const LABEL_IMAGE: &str = "bl.samcday.com/image";

// Annotations
pub const ANNOTATION_ADOPT: &str = "bl.samcday.com/adopt";

/// Labels set by the controller on every provisioned Node. User values from
/// `spec.template.metadata.labels` that collide with these keys are dropped.
pub const RESERVED_LABELS: &[&str] = &[
    LABEL_ASG,
    LABEL_SIZE,
    LABEL_REGION,
    LABEL_IMAGE,
    "kubernetes.io/hostname",
    "kubernetes.io/arch",
    "kubernetes.io/os",
    "node.kubernetes.io/instance-type",
    "node.kubernetes.io/cloud-provider",
    "topology.kubernetes.io/region",
];

/// Taint keys set by the controller on every provisioned Node. User taints
/// from `spec.template.spec.taints` whose key matches are dropped.
pub const RESERVED_TAINT_KEYS: &[&str] = &[UNINITIALIZED_TAINT];

pub fn node_password_secret_name(node_name: &str) -> String {
    format!("{node_name}-node-password")
}

pub fn user_data_secret_name(node_name: &str) -> String {
    format!("{node_name}-user-data")
}

pub struct ReconcileContext {
    pub bl: binarylane::Client,
    pub k8s: kube::Client,
    pub secret_namespace: String,
    pub bl_catalog: tokio::sync::RwLock<Option<BlCatalog>>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("kube: {0}")]
    Kube(#[from] kube::Error),
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

pub fn error_policy<T: kube::Resource>(
    _obj: Arc<T>,
    _err: &Error,
    _ctx: Arc<ReconcileContext>,
) -> Action {
    Action::requeue(Duration::from_secs(30))
}

pub fn secret_to_node_mapper(secret: Secret) -> Option<ObjectRef<Node>> {
    let name = secret.metadata.name.as_deref()?;
    let node_name = name
        .strip_suffix("-node-password")
        .or_else(|| name.strip_suffix("-user-data"))?;
    Some(ObjectRef::<Node>::new(node_name))
}

pub struct BlCatalog {
    pub sizes: Vec<binarylane::ListedSize>,
    pub regions: Vec<binarylane::ListedRegion>,
    pub images: Vec<binarylane::ListedImage>,
    pub fetched_at: std::time::Instant,
}

pub struct BlCatalogSnapshot {
    pub sizes: Vec<binarylane::ListedSize>,
    pub regions: Vec<binarylane::ListedRegion>,
    pub images: Vec<binarylane::ListedImage>,
}

impl ReconcileContext {
    pub async fn bl_catalog(&self) -> std::result::Result<BlCatalogSnapshot, Error> {
        {
            let guard = self.bl_catalog.read().await;
            if let Some(ref cat) = *guard
                && cat.fetched_at.elapsed() < Duration::from_secs(300)
            {
                return Ok(BlCatalogSnapshot {
                    sizes: cat.sizes.clone(),
                    regions: cat.regions.clone(),
                    images: cat.images.clone(),
                });
            }
        }

        // Take write lock to serialize refreshes — re-check in case another
        // task already refreshed while we were waiting for the lock.
        let mut guard = self.bl_catalog.write().await;
        if let Some(ref cat) = *guard
            && cat.fetched_at.elapsed() < Duration::from_secs(300)
        {
            return Ok(BlCatalogSnapshot {
                sizes: cat.sizes.clone(),
                regions: cat.regions.clone(),
                images: cat.images.clone(),
            });
        }

        let sizes = self.bl.list_sizes().await?;
        let regions = self.bl.list_regions().await?;
        let images = self.bl.list_images().await?;

        *guard = Some(BlCatalog {
            sizes: sizes.clone(),
            regions: regions.clone(),
            images: images.clone(),
            fetched_at: std::time::Instant::now(),
        });

        Ok(BlCatalogSnapshot {
            sizes,
            regions,
            images,
        })
    }
}

/// Default-enabled controllers.
const DEFAULT_CONTROLLERS: &[&str] = &[
    "node-sync",
    "node-deletion",
    "node-bind",
    "node-provision",
    "service",
];

/// Resolve a controller spec string into the set of enabled controller names.
///
/// Syntax (comma-separated):
/// - `*` expands to all default-enabled controllers
/// - `-name` excludes a controller
/// - `name` explicitly includes a controller
///
/// Examples:
/// - `"*"` → all defaults
/// - `"*,-service"` → all defaults except service
/// - `"*,-node-provision"` → all defaults except node-provision
/// - `"node-sync,node-bind"` → only those two
pub fn resolve_controllers(spec: &str) -> HashSet<String> {
    let mut result = HashSet::new();
    let mut exclusions = HashSet::new();

    for part in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if part == "*" {
            for &name in DEFAULT_CONTROLLERS {
                result.insert(name.to_string());
            }
        } else if let Some(excluded) = part.strip_prefix('-') {
            exclusions.insert(excluded.to_string());
        } else {
            result.insert(part.to_string());
        }
    }

    for name in &exclusions {
        result.remove(name);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_star() {
        let set = resolve_controllers("*");
        assert!(set.contains("node-sync"));
        assert!(set.contains("node-deletion"));
        assert!(set.contains("node-bind"));
        assert!(set.contains("node-provision"));
        assert!(set.contains("service"));
        assert!(!set.contains("autoscaler"));
        assert!(!set.contains("dns-webhook"));
    }

    #[test]
    fn test_resolve_star_minus() {
        let set = resolve_controllers("*,-service");
        assert!(set.contains("node-sync"));
        assert!(!set.contains("service"));
    }

    #[test]
    fn test_resolve_star_plus_explicit() {
        let set = resolve_controllers("*,autoscaler");
        assert!(set.contains("node-sync"));
        assert!(set.contains("autoscaler"));
    }

    #[test]
    fn test_resolve_explicit_only() {
        let set = resolve_controllers("node-sync,node-bind");
        assert_eq!(set.len(), 2);
        assert!(set.contains("node-sync"));
        assert!(set.contains("node-bind"));
    }

    #[test]
    fn test_secret_to_node_mapper() {
        let make_secret = |name: &str| Secret {
            metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                name: Some(name.to_string()),
                ..Default::default()
            },
            ..Default::default()
        };

        let ref_ = secret_to_node_mapper(make_secret("worker-1-node-password")).unwrap();
        assert_eq!(ref_.name, "worker-1");

        let ref_ = secret_to_node_mapper(make_secret("worker-1-user-data")).unwrap();
        assert_eq!(ref_.name, "worker-1");

        assert!(secret_to_node_mapper(make_secret("unrelated-secret")).is_none());
        assert!(secret_to_node_mapper(make_secret("node-password")).is_none());
    }
}
