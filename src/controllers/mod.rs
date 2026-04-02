pub mod node_bind;
pub mod node_deletion;
pub mod node_provision;
pub mod node_sync;
pub mod service;

use std::collections::HashSet;

use binarylane_client as binarylane;

pub const FINALIZER: &str = "blc.samcday.com/server-cleanup";
pub const UNINITIALIZED_TAINT: &str = "node.cloudprovider.kubernetes.io/uninitialized";

// Labels
pub const LABEL_SERVER_ID: &str = "bl.samcday.com/server-id";
pub const LABEL_SIZE: &str = "bl.samcday.com/size";
pub const LABEL_REGION: &str = "bl.samcday.com/region";
pub const LABEL_IMAGE: &str = "bl.samcday.com/image";

// Annotations
pub const ANNOTATION_ADOPT: &str = "bl.samcday.com/adopt";

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
/// - `"*,autoscaler"` → all defaults plus autoscaler
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
    fn test_resolve_star_plus_optin() {
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
}
