use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
    /// Reference to a Secret containing a shared password for all nodes.
    /// If omitted, a password is auto-generated into {asgName}-password.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password_secret_ref: Option<SecretRef>,
    /// Cloud-init user-data template (MiniJinja). Rendered per-node at scale-up.
    /// Built-in variables: `node.name`, `node.index`, `node.password`,
    /// `asg.name`, `asg.size`, `asg.region`, `asg.image`, `asg.namePrefix`,
    /// `asg.vcpus`, `asg.memoryMb`, `asg.diskGb`.
    /// User variables from `templateVariables` are available at the top level.
    /// Templating is evaluated at scale-up time only; Secret/ConfigMap changes
    /// do not re-render previously provisioned nodes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_data: Option<String>,
    /// Variables injected into the `userData` template, in addition to the
    /// built-in `node.*` and `asg.*` scopes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub template_variables: Vec<TemplateVariable>,
    /// Template applied to Nodes produced by this ASG. Labels/annotations/
    /// taints defined here are merged with controller-owned values at
    /// scale-up time; user values that collide with reserved keys are
    /// dropped and reported as a Warning Event on the ASG.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<NodeTemplate>,
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
pub struct NodeTemplate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<NodeTemplateMeta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<NodeTemplateSpec>,
}

#[derive(JsonSchema, Deserialize, Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct NodeTemplateMeta {
    /// Additional labels to apply to provisioned Nodes. Values that collide
    /// with controller-owned labels are dropped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<BTreeMap<String, String>>,
    /// Annotations to apply to provisioned Nodes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
}

#[derive(JsonSchema, Deserialize, Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct NodeTemplateSpec {
    /// Additional taints to apply to provisioned Nodes. Entries whose key
    /// collides with controller-owned taints are dropped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub taints: Option<Vec<NodeTaint>>,
}

#[derive(JsonSchema, Deserialize, Serialize, Clone, Debug)]
pub struct NodeTaint {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// One of: NoSchedule, PreferNoSchedule, NoExecute.
    pub effect: String,
}

#[derive(JsonSchema, Deserialize, Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TemplateVariable {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_from: Option<TemplateVariableSource>,
}

#[derive(JsonSchema, Deserialize, Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct TemplateVariableSource {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_key_ref: Option<KeySelector>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_map_key_ref: Option<KeySelector>,
}

#[derive(JsonSchema, Deserialize, Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct KeySelector {
    pub name: String,
    /// Namespace of the resource. Defaults to the controller's namespace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub key: String,
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
