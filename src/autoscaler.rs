use std::collections::HashMap;
use std::sync::Arc;

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
    cfg: Config,
    servers: Arc<RwLock<HashMap<i64, binarylane::Server>>>,
}

impl Provider {
    pub fn new(bl: BlClient, cfg: Config) -> Self {
        Self {
            bl,
            cfg,
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

        // NOTE: if a creation fails mid-loop, already-created servers remain.
        // The autoscaler's next refresh() will pick them up, but a retry before
        // refresh could overshoot max_size. Acceptable for now since the BL API
        // will reject servers beyond account quota anyway.
        for i in 0..req.delta {
            let ts = chrono_like_timestamp();
            let name = format!("{}{}-{ts}-{i}", self.cfg.name_prefix, ng.id);

            let mut vars = self.cfg.template_vars.clone();
            vars.insert("NodeName".to_string(), name.clone());
            vars.insert("NodeGroup".to_string(), ng.id.clone());
            vars.insert("Region".to_string(), ng.region.clone());
            vars.insert("Size".to_string(), ng.size.clone());
            let user_data = self.render_cloud_init(&vars);

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
                })
                .await
                .map_err(|e| Status::internal(format!("creating server: {e}")))?;

            self.servers.write().await.insert(srv.id, srv.clone());
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

        // Build a K8s Node JSON and serialize it
        let mut labels = serde_json::Map::new();
        labels.insert(
            "node.kubernetes.io/instance-type".to_string(),
            serde_json::Value::String(ng.size.clone()),
        );
        labels.insert(
            "topology.kubernetes.io/region".to_string(),
            serde_json::Value::String(ng.region.clone()),
        );
        labels.insert(
            "kubernetes.io/arch".to_string(),
            serde_json::Value::String("amd64".to_string()),
        );
        labels.insert(
            "kubernetes.io/os".to_string(),
            serde_json::Value::String("linux".to_string()),
        );
        labels.insert(
            "node.kubernetes.io/cloud-provider".to_string(),
            serde_json::Value::String(binarylane::PROVIDER_NAME.to_string()),
        );
        for (k, v) in &ng.labels {
            labels.insert(k.clone(), serde_json::Value::String(v.clone()));
        }

        let node = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Node",
            "metadata": {
                "name": format!("template-{}", ng.id),
                "labels": labels,
            },
            "spec": {
                "providerID": format!("{PROVIDER_NAME}:///template", PROVIDER_NAME = binarylane::PROVIDER_NAME),
                "taints": [{
                    "key": "node.cloudprovider.kubernetes.io/uninitialized",
                    "value": "true",
                    "effect": "NoSchedule",
                }],
            },
            "status": {
                "capacity": {
                    "cpu": format!("{}", ng.vcpus),
                    "memory": format!("{}Mi", ng.memory_mb),
                    "ephemeral-storage": format!("{}Gi", ng.disk_gb),
                    "pods": "110",
                },
                "allocatable": {
                    "cpu": format!("{}m", (ng.vcpus * 1000).saturating_sub(100)),
                    "memory": format!("{}Mi", ng.memory_mb.saturating_sub(256)),
                    "ephemeral-storage": format!("{}Gi", ng.disk_gb.saturating_sub(1)),
                    "pods": "110",
                },
                "conditions": [{
                    "type": "Ready",
                    "status": "True",
                }],
            },
        });

        let node_bytes = serde_json::to_vec(&node)
            .map_err(|e| Status::internal(format!("serializing template node: {e}")))?;
        Ok(Response::new(proto::NodeGroupTemplateNodeInfoResponse {
            node_info: node_bytes,
        }))
    }

    async fn node_group_get_options(
        &self,
        _req: Request<proto::NodeGroupAutoscalingOptionsRequest>,
    ) -> Result<Response<proto::NodeGroupAutoscalingOptionsResponse>, Status> {
        Err(Status::unimplemented("not implemented"))
    }
}

fn chrono_like_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    // Millisecond granularity avoids name collisions from rapid successive calls
    format!("{}", dur.as_millis())
}
