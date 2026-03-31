use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use k8s_openapi::api::core::v1::{
    Event as K8sEvent, EventSource, Node as K8sNode, NodeSpec as K8sNodeSpec, ObjectReference,
    Secret as K8sSecret, Taint as K8sTaint,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta as K8sObjectMeta, Time};
use k8s_pb::api::core::v1::{Node, NodeCondition, NodeSpec, NodeStatus, Taint};
use k8s_pb::apimachinery::pkg::api::resource::Quantity;
use k8s_pb::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::Api;
use kube::api::{Patch, PatchParams};
use prost_014::Message;
use rand::{Rng, distributions::Alphanumeric, thread_rng};
use serde::Deserialize;
use tokio::sync::RwLock;
use tonic::{Request, Response, Status};
use tracing::info;

use crate::binarylane::{self, Client as BlClient, CreateServerRequest};
use crate::proto;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeGroupConfig {
    pub id: String,
    pub min_size: i32,
    pub max_size: i32,
    pub size: String,
    pub region: String,
    pub image: String,
    pub vcpus: i32,
    pub memory_mb: i32,
    pub disk_gb: i32,
    #[serde(default)]
    pub labels: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub node_groups: Vec<NodeGroupConfig>,
    #[serde(skip)]
    pub cloud_init: String,
    #[serde(default)]
    pub ssh_keys: Vec<String>,
    #[serde(default)]
    pub name_prefix: String,
    #[serde(skip)]
    pub template_vars: HashMap<String, String>,
}

pub struct Provider {
    bl: BlClient,
    k8s: kube::Client,
    cfg: Config,
    secret_namespace: String,
    servers: Arc<RwLock<HashMap<i64, binarylane::Server>>>,
}

impl Provider {
    pub fn new(bl: BlClient, k8s: kube::Client, cfg: Config, secret_namespace: String) -> Self {
        Self {
            bl,
            k8s,
            cfg,
            secret_namespace,
            servers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn find_group(&self, id: &str) -> Option<&NodeGroupConfig> {
        self.cfg.node_groups.iter().find(|ng| ng.id == id)
    }

    async fn servers_for_group(&self, group_id: &str) -> Vec<binarylane::Server> {
        let prefix = format!("{}{group_id}-", self.cfg.name_prefix);
        let servers = self.servers.read().await;
        servers
            .values()
            .filter(|s| s.name.starts_with(&prefix))
            .cloned()
            .collect()
    }

    fn render_cloud_init(&self, vars: &HashMap<String, String>) -> String {
        let mut result = self.cfg.cloud_init.clone();
        for (k, v) in vars {
            result = result.replace(&format!("{{{{.{k}}}}}"), v);
        }
        result
    }

    fn node_labels(&self, ng: &NodeGroupConfig) -> BTreeMap<String, String> {
        let mut labels = BTreeMap::new();
        labels.insert(
            "node.kubernetes.io/instance-type".to_string(),
            ng.size.clone(),
        );
        labels.insert(
            "topology.kubernetes.io/region".to_string(),
            ng.region.clone(),
        );
        // TODO: derive from size/config if BinaryLane adds ARM instances
        labels.insert("kubernetes.io/arch".to_string(), "amd64".to_string());
        labels.insert("kubernetes.io/os".to_string(), "linux".to_string());
        labels.insert(
            "node.kubernetes.io/cloud-provider".to_string(),
            binarylane::PROVIDER_NAME.to_string(),
        );
        labels.extend(ng.labels.clone());
        labels
    }

    async fn upsert_node_password_secret(
        &self,
        node_name: &str,
        server_id: i64,
        password: &str,
    ) -> Result<(), Status> {
        let secrets_api: Api<K8sSecret> = Api::namespaced(self.k8s.clone(), &self.secret_namespace);
        let secret_name = crate::node_controller::node_password_secret_name(node_name);
        let patch = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": {
                "name": secret_name,
                "labels": {
                    "app.kubernetes.io/managed-by": "binarylane-controller",
                    "bl.samcday.com/node-name": node_name,
                },
                "annotations": {
                    "bl.samcday.com/server-id": server_id.to_string(),
                },
            },
            "type": "Opaque",
            "stringData": {
                "password": password,
            },
        });

        secrets_api
            .patch(
                &secret_name,
                &PatchParams::apply("binarylane-controller").force(),
                &Patch::Apply(&patch),
            )
            .await
            .map_err(|e| {
                Status::internal(format!(
                    "upserting node password secret {}/{}: {e}",
                    self.secret_namespace, secret_name
                ))
            })?;

        Ok(())
    }
}

#[tonic::async_trait]
impl proto::cloud_provider_server::CloudProvider for Provider {
    async fn node_groups(
        &self,
        _req: Request<proto::NodeGroupsRequest>,
    ) -> Result<Response<proto::NodeGroupsResponse>, Status> {
        let groups = self
            .cfg
            .node_groups
            .iter()
            .map(|ng| proto::NodeGroup {
                id: ng.id.clone(),
                min_size: ng.min_size,
                max_size: ng.max_size,
                debug: format!("BinaryLane {} in {} (size: {})", ng.id, ng.region, ng.size),
            })
            .collect();
        Ok(Response::new(proto::NodeGroupsResponse {
            node_groups: groups,
        }))
    }

    async fn node_group_for_node(
        &self,
        req: Request<proto::NodeGroupForNodeRequest>,
    ) -> Result<Response<proto::NodeGroupForNodeResponse>, Status> {
        let req = req.into_inner();
        let node = match req.node {
            Some(n) => n,
            None => {
                return Ok(Response::new(proto::NodeGroupForNodeResponse {
                    node_group: None,
                }));
            }
        };
        let server_id = match binarylane::parse_provider_id(&node.provider_id) {
            Some(id) => id,
            None => {
                return Ok(Response::new(proto::NodeGroupForNodeResponse {
                    node_group: None,
                }));
            }
        };
        let servers = self.servers.read().await;
        let srv = match servers.get(&server_id) {
            Some(s) => s,
            None => {
                return Ok(Response::new(proto::NodeGroupForNodeResponse {
                    node_group: None,
                }));
            }
        };
        for ng in &self.cfg.node_groups {
            let prefix = format!("{}{}-", self.cfg.name_prefix, ng.id);
            if srv.name.starts_with(&prefix) {
                return Ok(Response::new(proto::NodeGroupForNodeResponse {
                    node_group: Some(proto::NodeGroup {
                        id: ng.id.clone(),
                        min_size: ng.min_size,
                        max_size: ng.max_size,
                        debug: String::new(),
                    }),
                }));
            }
        }
        Ok(Response::new(proto::NodeGroupForNodeResponse {
            node_group: None,
        }))
    }

    async fn refresh(
        &self,
        _req: Request<proto::RefreshRequest>,
    ) -> Result<Response<proto::RefreshResponse>, Status> {
        let all_servers = self
            .bl
            .list_servers()
            .await
            .map_err(|e| Status::internal(format!("refreshing servers: {e}")))?;
        let mut servers = self.servers.write().await;
        servers.clear();
        for s in all_servers {
            if s.name.starts_with(&self.cfg.name_prefix) {
                servers.insert(s.id, s);
            }
        }
        info!(managed_count = servers.len(), "refreshed server list");
        Ok(Response::new(proto::RefreshResponse {}))
    }

    async fn cleanup(
        &self,
        _req: Request<proto::CleanupRequest>,
    ) -> Result<Response<proto::CleanupResponse>, Status> {
        Ok(Response::new(proto::CleanupResponse {}))
    }

    async fn gpu_label(
        &self,
        _req: Request<proto::GpuLabelRequest>,
    ) -> Result<Response<proto::GpuLabelResponse>, Status> {
        Ok(Response::new(proto::GpuLabelResponse {
            label: String::new(),
        }))
    }

    async fn get_available_gpu_types(
        &self,
        _req: Request<proto::GetAvailableGpuTypesRequest>,
    ) -> Result<Response<proto::GetAvailableGpuTypesResponse>, Status> {
        Ok(Response::new(proto::GetAvailableGpuTypesResponse {
            gpu_types: HashMap::new(),
        }))
    }

    async fn pricing_node_price(
        &self,
        _req: Request<proto::PricingNodePriceRequest>,
    ) -> Result<Response<proto::PricingNodePriceResponse>, Status> {
        Err(Status::unimplemented("not implemented"))
    }

    async fn pricing_pod_price(
        &self,
        _req: Request<proto::PricingPodPriceRequest>,
    ) -> Result<Response<proto::PricingPodPriceResponse>, Status> {
        Err(Status::unimplemented("not implemented"))
    }

    async fn node_group_target_size(
        &self,
        req: Request<proto::NodeGroupTargetSizeRequest>,
    ) -> Result<Response<proto::NodeGroupTargetSizeResponse>, Status> {
        let id = &req.into_inner().id;
        if self.find_group(id).is_none() {
            return Err(Status::not_found(format!("node group {id} not found")));
        }
        let count = self.servers_for_group(id).await.len() as i32;
        Ok(Response::new(proto::NodeGroupTargetSizeResponse {
            target_size: count,
        }))
    }

    async fn node_group_increase_size(
        &self,
        req: Request<proto::NodeGroupIncreaseSizeRequest>,
    ) -> Result<Response<proto::NodeGroupIncreaseSizeResponse>, Status> {
        let req = req.into_inner();
        if req.delta <= 0 {
            return Err(Status::invalid_argument(format!(
                "delta must be positive, got {}",
                req.delta
            )));
        }
        let ng = self
            .find_group(&req.id)
            .ok_or_else(|| Status::not_found(format!("node group {} not found", req.id)))?
            .clone();
        let current = self.servers_for_group(&req.id).await;
        if req.delta as usize + current.len() > ng.max_size as usize {
            return Err(Status::invalid_argument(format!(
                "increase would exceed max size {}",
                ng.max_size
            )));
        }

        let images = self
            .bl
            .list_images()
            .await
            .map_err(|e| Status::internal(format!("listing images: {e}")))?;
        if !images
            .iter()
            .any(|image| image.slug.as_deref() == Some(ng.image.as_str()))
        {
            return Err(Status::invalid_argument(format!(
                "image slug '{}' not found in BinaryLane image list",
                ng.image
            )));
        }

        // NOTE: if a creation fails mid-loop, already-created servers remain.
        // The autoscaler's next refresh() will pick them up, but a retry before
        // refresh could overshoot max_size. Acceptable for now since the BL API
        // will reject servers beyond account quota anyway.
        let node_api: Api<K8sNode> = Api::all(self.k8s.clone());
        let events_api: Api<K8sEvent> = Api::namespaced(self.k8s.clone(), "default");
        for i in 0..req.delta {
            let ts = chrono_like_timestamp();
            let name = format!("{}{}-{ts}-{i}", self.cfg.name_prefix, ng.id);

            let mut vars = self.cfg.template_vars.clone();
            vars.insert("NodeName".to_string(), name.clone());
            vars.insert("NodeGroup".to_string(), ng.id.clone());
            vars.insert("Region".to_string(), ng.region.clone());
            vars.insert("Size".to_string(), ng.size.clone());
            let user_data = self.render_cloud_init(&vars);
            let server_password = generate_server_password();

            info!(name = %name, size = %ng.size, region = %ng.region, "creating server");
            let srv = self
                .bl
                .create_server(CreateServerRequest {
                    name: name.clone(),
                    size: ng.size.clone(),
                    image: ng.image.clone(),
                    region: ng.region.clone(),
                    user_data: Some(user_data),
                    ssh_keys: if self.cfg.ssh_keys.is_empty() {
                        None
                    } else {
                        Some(self.cfg.ssh_keys.clone())
                    },
                    password: Some(server_password.clone()),
                })
                .await
                .map_err(|e| Status::internal(format!("creating server: {e}")))?;

            self.servers.write().await.insert(srv.id, srv.clone());
            self.upsert_node_password_secret(&name, srv.id, &server_password)
                .await?;

            let mut labels = self.node_labels(&ng);
            labels.insert("kubernetes.io/hostname".to_string(), name.clone());
            let node = K8sNode {
                metadata: K8sObjectMeta {
                    name: Some(name.clone()),
                    labels: Some(labels),
                    finalizers: Some(vec![crate::node_controller::FINALIZER.to_string()]),
                    ..Default::default()
                },
                spec: Some(K8sNodeSpec {
                    provider_id: Some(binarylane::server_provider_id(srv.id)),
                    taints: Some(vec![K8sTaint {
                        key: "node.cloudprovider.kubernetes.io/uninitialized".to_string(),
                        value: Some("true".to_string()),
                        effect: "NoSchedule".to_string(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                status: None,
            };
            let node_uid = match node_api.create(&Default::default(), &node).await {
                Ok(created) => {
                    info!(name = %name, id = srv.id, "pre-created k8s node");
                    created.metadata.uid
                }
                Err(kube::Error::Api(err)) if err.code == 409 => {
                    info!(name = %name, id = srv.id, "k8s node already exists");
                    node_api.get(&name).await.ok().and_then(|n| n.metadata.uid)
                }
                Err(e) => {
                    tracing::warn!(name = %name, id = srv.id, error = %e, "failed to pre-create k8s node");
                    None
                }
            };

            // Use core v1 Event (not events.k8s.io/v1) with source/involvedObject
            // fields — this is what kubelet and node-controller use, and what k9s
            // filters on when showing events for a Node.
            let now = Time(k8s_openapi::chrono::Utc::now());
            let event = K8sEvent {
                metadata: K8sObjectMeta {
                    generate_name: Some("binarylane-controller-".to_string()),
                    namespace: Some("default".to_string()),
                    ..Default::default()
                },
                involved_object: ObjectReference {
                    api_version: Some("v1".to_string()),
                    kind: Some("Node".to_string()),
                    name: Some(name.clone()),
                    uid: node_uid,
                    ..Default::default()
                },
                reason: Some("ServerCreated".to_string()),
                message: Some(format!(
                    "Created BinaryLane server {} ({}, {})",
                    srv.id, ng.size, ng.region
                )),
                type_: Some("Normal".to_string()),
                source: Some(EventSource {
                    component: Some("binarylane-controller".to_string()),
                    ..Default::default()
                }),
                first_timestamp: Some(now.clone()),
                last_timestamp: Some(now),
                count: Some(1),
                action: Some("ScaleUp".to_string()),
                ..Default::default()
            };
            if let Err(e) = events_api.create(&Default::default(), &event).await {
                tracing::warn!(
                    name = %name,
                    id = srv.id,
                    error = %e,
                    "failed to emit server created event"
                );
            }

            info!(name = %name, id = srv.id, "created server");
        }

        Ok(Response::new(proto::NodeGroupIncreaseSizeResponse {}))
    }

    async fn node_group_delete_nodes(
        &self,
        req: Request<proto::NodeGroupDeleteNodesRequest>,
    ) -> Result<Response<proto::NodeGroupDeleteNodesResponse>, Status> {
        let req = req.into_inner();
        if self.find_group(&req.id).is_none() {
            return Err(Status::not_found(format!(
                "node group {} not found",
                req.id
            )));
        }
        for node in &req.nodes {
            let server_id = binarylane::parse_provider_id(&node.provider_id).ok_or_else(|| {
                Status::invalid_argument(format!("invalid provider ID: {}", node.provider_id))
            })?;
            info!(id = server_id, "deleting server");
            self.bl
                .delete_server(server_id)
                .await
                .map_err(|e| Status::internal(format!("deleting server: {e}")))?;
            self.servers.write().await.remove(&server_id);
        }
        Ok(Response::new(proto::NodeGroupDeleteNodesResponse {}))
    }

    // Intentional no-op: actual node removal is handled by node_group_delete_nodes.
    // decrease_target_size only adjusts the target count, which we derive from the
    // actual server list, so there's nothing to do here.
    async fn node_group_decrease_target_size(
        &self,
        req: Request<proto::NodeGroupDecreaseTargetSizeRequest>,
    ) -> Result<Response<proto::NodeGroupDecreaseTargetSizeResponse>, Status> {
        let id = &req.into_inner().id;
        if self.find_group(id).is_none() {
            return Err(Status::not_found(format!("node group {id} not found")));
        }
        Ok(Response::new(proto::NodeGroupDecreaseTargetSizeResponse {}))
    }

    async fn node_group_nodes(
        &self,
        req: Request<proto::NodeGroupNodesRequest>,
    ) -> Result<Response<proto::NodeGroupNodesResponse>, Status> {
        let id = &req.into_inner().id;
        if self.find_group(id).is_none() {
            return Err(Status::not_found(format!("node group {id} not found")));
        }
        let servers = self.servers_for_group(id).await;
        let instances = servers
            .iter()
            .map(|s| {
                let state = match s.status.as_str() {
                    "active" => proto::instance_status::InstanceState::InstanceRunning as i32,
                    "new" => proto::instance_status::InstanceState::InstanceCreating as i32,
                    _ => proto::instance_status::InstanceState::Unspecified as i32,
                };
                proto::Instance {
                    id: binarylane::server_provider_id(s.id),
                    status: Some(proto::InstanceStatus {
                        instance_state: state,
                        error_info: None,
                    }),
                }
            })
            .collect();
        Ok(Response::new(proto::NodeGroupNodesResponse { instances }))
    }

    async fn node_group_template_node_info(
        &self,
        req: Request<proto::NodeGroupTemplateNodeInfoRequest>,
    ) -> Result<Response<proto::NodeGroupTemplateNodeInfoResponse>, Status> {
        let id = &req.into_inner().id;
        let ng = self
            .find_group(id)
            .ok_or_else(|| Status::not_found(format!("node group {id} not found")))?;

        let mut labels = self.node_labels(ng);
        labels.insert(
            "kubernetes.io/hostname".to_string(),
            format!("template-{}", ng.id),
        );

        let mut capacity = BTreeMap::new();
        capacity.insert(
            "cpu".to_string(),
            Quantity {
                string: Some(ng.vcpus.to_string()),
            },
        );
        capacity.insert(
            "memory".to_string(),
            Quantity {
                string: Some(format!("{}Mi", ng.memory_mb)),
            },
        );
        capacity.insert(
            "ephemeral-storage".to_string(),
            Quantity {
                string: Some(format!("{}Gi", ng.disk_gb)),
            },
        );
        capacity.insert(
            "pods".to_string(),
            Quantity {
                string: Some("110".to_string()),
            },
        );

        let mut allocatable = BTreeMap::new();
        allocatable.insert(
            "cpu".to_string(),
            Quantity {
                string: Some(format!("{}m", (ng.vcpus * 1000).saturating_sub(100))),
            },
        );
        allocatable.insert(
            "memory".to_string(),
            Quantity {
                string: Some(format!("{}Mi", ng.memory_mb.saturating_sub(256))),
            },
        );
        allocatable.insert(
            "ephemeral-storage".to_string(),
            Quantity {
                string: Some(format!("{}Gi", ng.disk_gb.saturating_sub(1))),
            },
        );
        allocatable.insert(
            "pods".to_string(),
            Quantity {
                string: Some("110".to_string()),
            },
        );

        let node = Node {
            metadata: Some(ObjectMeta {
                name: Some(format!("template-{}", ng.id)),
                labels,
                ..Default::default()
            }),
            spec: Some(NodeSpec {
                provider_id: Some(format!("{}:///template", binarylane::PROVIDER_NAME)),
                taints: vec![Taint {
                    key: Some("node.cloudprovider.kubernetes.io/uninitialized".to_string()),
                    value: Some("true".to_string()),
                    effect: Some("NoSchedule".to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            status: Some(NodeStatus {
                capacity,
                allocatable,
                conditions: vec![NodeCondition {
                    r#type: Some("Ready".to_string()),
                    status: Some("True".to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
        };

        let node_bytes = node.encode_to_vec();
        Ok(Response::new(proto::NodeGroupTemplateNodeInfoResponse {
            node_bytes,
        }))
    }

    async fn node_group_get_options(
        &self,
        _req: Request<proto::NodeGroupAutoscalingOptionsRequest>,
    ) -> Result<Response<proto::NodeGroupAutoscalingOptionsResponse>, Status> {
        Err(Status::unimplemented("not implemented"))
    }
}

fn generate_server_password() -> String {
    let mut rng = thread_rng();
    let suffix: String = (&mut rng)
        .sample_iter(Alphanumeric)
        .take(36)
        .map(char::from)
        .collect();
    format!("BlcA1{suffix}")
}

fn chrono_like_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    // Millisecond granularity avoids name collisions from rapid successive calls
    format!("{}", dur.as_millis())
}
