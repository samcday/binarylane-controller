use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(CustomResource, JsonSchema, Deserialize, Serialize, Clone, Debug)]
#[kube(
    group = "blc.samcday.com",
    version = "v1alpha1",
    kind = "AutoScalingGroup",
    root = "AutoScalingGroup",
    status = "AutoScalingGroupStatus",
    shortname = "asg",
    namespaced = false,
    printcolumn = r#"{"name":"Min","type":"integer","jsonPath":".spec.minSize"}"#,
    printcolumn = r#"{"name":"Max","type":"integer","jsonPath":".spec.maxSize"}"#,
    printcolumn = r#"{"name":"Replicas","type":"integer","jsonPath":".status.replicas"}"#,
    printcolumn = r#"{"name":"Last Scale Up","type":"date","jsonPath":".status.lastScaleUp"}"#,
    printcolumn = r#"{"name":"Last Scale Down","type":"date","jsonPath":".status.lastScaleDown"}"#,
    printcolumn = r#"{"name":"Size","type":"string","jsonPath":".spec.size","priority":1}"#,
    printcolumn = r#"{"name":"Region","type":"string","jsonPath":".spec.region","priority":1}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp","priority":1}"#
)]
#[serde(rename_all = "camelCase")]
pub struct AutoScalingGroupSpec {
    pub min_size: i32,
    pub max_size: i32,
    /// BinaryLane size slug
    pub size: String,
    /// BinaryLane region slug
    pub region: String,
    /// BinaryLane image slug
    pub image: String,
    /// Override vCPU count (defaults from BL size catalog if omitted)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vcpus: Option<i32>,
    /// Override memory in MB (defaults from BL size catalog if omitted)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<i32>,
    /// Override disk in GB (defaults from BL size catalog if omitted)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_gb: Option<i32>,
    /// Node name prefix. Nodes named {namePrefix}{asgName}-{ts}-{idx}
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name_prefix: String,
    /// Reference to a Secret containing user-data for provisioned nodes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_data_secret_ref: Option<SecretRef>,
    /// Reference to a Secret containing a shared password for all nodes.
    /// If omitted, a password is auto-generated into {asgName}-password.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password_secret_ref: Option<SecretRef>,
}

#[derive(JsonSchema, Deserialize, Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct SecretRef {
    pub name: String,
    /// Namespace of the Secret. Defaults to the controller's namespace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Key within the Secret. Defaults to "user-data" or "password" depending on context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[derive(JsonSchema, Deserialize, Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct AutoScalingGroupStatus {
    /// Current number of nodes in this group
    #[serde(default)]
    pub replicas: i32,
    /// Timestamp of the last scale-up event
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_scale_up: Option<chrono::DateTime<chrono::Utc>>,
    /// Timestamp of the last scale-down event
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_scale_down: Option<chrono::DateTime<chrono::Utc>>,
}
