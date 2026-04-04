use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use k8s_openapi::api::core::v1::{
    Node as K8sNode, NodeSpec as K8sNodeSpec, Secret as K8sSecret, Taint as K8sTaint,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta as K8sObjectMeta;
use k8s_pb::api::core::v1::{Node, NodeCondition, NodeSpec, NodeStatus, Taint};
use k8s_pb::apimachinery::pkg::api::resource::Quantity;
use k8s_pb::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::Api;
use kube::api::{Patch, PatchParams};
use kube::runtime::reflector::Store;
use prost_014::Message;
use tonic::{Request, Response, Status};
use tracing::{info, warn};

use crate::binarylane;
use crate::controllers;
use crate::crd::{AutoScalingGroup, AutoScalingGroupSpec, AutoScalingGroupStatus, SecretRef};
use crate::proto;

pub struct Provider {
    k8s: kube::Client,
    bl: binarylane::Client,
    store: Store<AutoScalingGroup>,
    secret_namespace: String,
    size_cache: tokio::sync::Mutex<Option<Vec<binarylane::ListedSize>>>,
}

impl Provider {
    pub fn new(
        k8s: kube::Client,
        bl: binarylane::Client,
        store: Store<AutoScalingGroup>,
        secret_namespace: String,
    ) -> Self {
        Self {
            k8s,
            bl,
            store,
            secret_namespace,
            size_cache: tokio::sync::Mutex::new(None),
        }
    }

    fn find_asg(&self, id: &str) -> Option<Arc<AutoScalingGroup>> {
        self.store
            .state()
            .into_iter()
            .find(|asg| asg.metadata.name.as_deref() == Some(id))
    }

    async fn get_sizes(&self) -> Result<Vec<binarylane::ListedSize>, Status> {
        let mut cache = self.size_cache.lock().await;
        if let Some(sizes) = cache.as_ref() {
            return Ok(sizes.clone());
        }
        let sizes = self
            .bl
            .list_sizes()
            .await
            .map_err(|e| Status::internal(format!("listing BinaryLane sizes: {e}")))?;
        *cache = Some(sizes.clone());
        Ok(sizes)
    }

    /// Lists K8s nodes belonging to a node group, excluding those being deleted.
    async fn nodes_for_group(&self, asg: &AutoScalingGroup) -> Result<Vec<K8sNode>, Status> {
        let asg_name = asg.metadata.name.as_deref().unwrap_or("");
        let nodes_api: Api<K8sNode> = Api::all(self.k8s.clone());
        let lp =
            kube::api::ListParams::default().labels(&format!("blc.samcday.com/asg={asg_name}"));
        let node_list = nodes_api
            .list(&lp)
            .await
            .map_err(|e| Status::internal(format!("listing nodes: {e}")))?;
        Ok(node_list
            .items
            .into_iter()
            .filter(|n| n.metadata.deletion_timestamp.is_none())
            .collect())
    }

    fn node_labels(&self, asg_name: &str, spec: &AutoScalingGroupSpec) -> BTreeMap<String, String> {
        let mut labels = BTreeMap::new();
        labels.insert("blc.samcday.com/asg".to_string(), asg_name.to_string());
        labels.insert(
            "node.kubernetes.io/instance-type".to_string(),
            spec.size.clone(),
        );
        labels.insert(
            "topology.kubernetes.io/region".to_string(),
            spec.region.clone(),
        );
        // TODO: derive from size/config if BinaryLane adds ARM instances
        labels.insert("kubernetes.io/arch".to_string(), "amd64".to_string());
        labels.insert("kubernetes.io/os".to_string(), "linux".to_string());
        labels.insert(
            "node.kubernetes.io/cloud-provider".to_string(),
            binarylane::PROVIDER_NAME.to_string(),
        );
        labels
    }

    async fn read_secret_value(
        &self,
        secret_ref: &SecretRef,
        default_key: &str,
    ) -> Result<String, Status> {
        let namespace = secret_ref
            .namespace
            .as_deref()
            .unwrap_or(&self.secret_namespace);
        let key = secret_ref.key.as_deref().unwrap_or(default_key);
        let secrets_api: Api<K8sSecret> = Api::namespaced(self.k8s.clone(), namespace);
        let secret = secrets_api
            .get(&secret_ref.name)
            .await
            .map_err(|e| match e {
                kube::Error::Api(err) if err.code == 404 => Status::not_found(format!(
                    "secret {}/{} not found",
                    namespace, secret_ref.name
                )),
                other => Status::internal(format!(
                    "reading secret {}/{}: {other}",
                    namespace, secret_ref.name
                )),
            })?;

        let data = secret.data.ok_or_else(|| {
            Status::internal(format!(
                "secret {}/{} has no data",
                namespace, secret_ref.name
            ))
        })?;
        let bytes = data.get(key).ok_or_else(|| {
            Status::internal(format!(
                "secret {}/{} missing key {}",
                namespace, secret_ref.name, key
            ))
        })?;

        String::from_utf8(bytes.0.clone()).map_err(|e| {
            Status::internal(format!(
                "secret {}/{} key {} is not valid UTF-8: {e}",
                namespace, secret_ref.name, key
            ))
        })
    }

    async fn ensure_password(&self, asg: &AutoScalingGroup) -> Result<String, Status> {
        if let Some(secret_ref) = &asg.spec.password_secret_ref {
            return self.read_secret_value(secret_ref, "password").await;
        }

        let asg_name = asg
            .metadata
            .name
            .as_deref()
            .ok_or_else(|| Status::invalid_argument("asg missing metadata.name".to_string()))?;
        let secret_name = format!("{asg_name}-password");
        let secrets_api: Api<K8sSecret> = Api::namespaced(self.k8s.clone(), &self.secret_namespace);
        let password = binarylane::generate_server_password();
        let secret = K8sSecret {
            metadata: K8sObjectMeta {
                name: Some(secret_name.clone()),
                namespace: Some(self.secret_namespace.clone()),
                labels: Some(BTreeMap::from([(
                    "app.kubernetes.io/managed-by".to_string(),
                    "binarylane-controller".to_string(),
                )])),
                ..Default::default()
            },
            string_data: Some(BTreeMap::from([("password".to_string(), password.clone())])),
            ..Default::default()
        };

        match secrets_api.create(&Default::default(), &secret).await {
            Ok(_) => Ok(password),
            Err(kube::Error::Api(err)) if err.code == 409 => {
                let existing = secrets_api.get(&secret_name).await.map_err(|e| {
                    Status::internal(format!(
                        "reading secret {}/{}: {e}",
                        self.secret_namespace, secret_name
                    ))
                })?;
                let data = existing.data.ok_or_else(|| {
                    Status::internal(format!(
                        "secret {}/{} has no data",
                        self.secret_namespace, secret_name
                    ))
                })?;
                let bytes = data.get("password").ok_or_else(|| {
                    Status::internal(format!(
                        "secret {}/{} missing key password",
                        self.secret_namespace, secret_name
                    ))
                })?;
                String::from_utf8(bytes.0.clone()).map_err(|e| {
                    Status::internal(format!(
                        "secret {}/{} key password is not valid UTF-8: {e}",
                        self.secret_namespace, secret_name
                    ))
                })
            }
            Err(e) => Err(Status::internal(format!(
                "creating secret {}/{}: {e}",
                self.secret_namespace, secret_name
            ))),
        }
    }

    async fn patch_asg_status(
        &self,
        name: &str,
        status: AutoScalingGroupStatus,
    ) -> Result<(), Status> {
        let asg_api: Api<AutoScalingGroup> = Api::all(self.k8s.clone());
        let patch = serde_json::json!({
            "apiVersion": "blc.samcday.com/v1alpha1",
            "kind": "AutoScalingGroup",
            "metadata": { "name": name },
            "status": status,
        });
        asg_api
            .patch_status(
                name,
                &PatchParams::apply("binarylane-controller").force(),
                &Patch::Apply(&patch),
            )
            .await
            .map_err(|e| Status::internal(format!("updating ASG status {name}: {e}")))?;
        Ok(())
    }

    async fn create_secret(
        &self,
        name: &str,
        data_key: &str,
        data_value: &str,
    ) -> Result<(), Status> {
        let secrets_api: Api<K8sSecret> = Api::namespaced(self.k8s.clone(), &self.secret_namespace);
        let patch = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": {
                "name": name,
                "labels": {
                    "app.kubernetes.io/managed-by": "binarylane-controller",
                },
            },
            "type": "Opaque",
            "stringData": {
                (data_key): data_value,
            },
        });

        secrets_api
            .patch(
                name,
                &PatchParams::apply("binarylane-controller").force(),
                &Patch::Apply(&patch),
            )
            .await
            .map_err(|e| {
                Status::internal(format!(
                    "creating secret {}/{}: {e}",
                    self.secret_namespace, name
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
            .store
            .state()
            .into_iter()
            .filter_map(|asg| {
                let name = asg.metadata.name.clone()?;
                Some(proto::NodeGroup {
                    id: name.clone(),
                    min_size: asg.spec.min_size,
                    max_size: asg.spec.max_size,
                    debug: format!(
                        "BinaryLane {} in {} (size: {})",
                        name, asg.spec.region, asg.spec.size
                    ),
                })
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
        for asg in self.store.state() {
            let asg_name = match asg.metadata.name.clone() {
                Some(name) => name,
                None => continue,
            };
            let prefix = format!("{}{}-", asg.spec.name_prefix, asg_name);
            if node.name.starts_with(&prefix) {
                return Ok(Response::new(proto::NodeGroupForNodeResponse {
                    node_group: Some(proto::NodeGroup {
                        id: asg_name,
                        min_size: asg.spec.min_size,
                        max_size: asg.spec.max_size,
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
        let asgs = self.store.state();
        {
            let mut cache = self.size_cache.lock().await;
            match self.bl.list_sizes().await {
                Ok(sizes) => *cache = Some(sizes),
                Err(e) => warn!(error = %e, "failed to refresh BinaryLane size cache"),
            }
        }
        let asg_api: Api<AutoScalingGroup> = Api::all(self.k8s.clone());
        for asg in &asgs {
            let count = self.nodes_for_group(asg).await?.len() as i32;
            let name = asg.metadata.name.as_deref().unwrap_or("");
            if name.is_empty() {
                continue;
            }
            let status = AutoScalingGroupStatus {
                replicas: count,
                ..asg.status.clone().unwrap_or_default()
            };
            let patch = serde_json::json!({
                "apiVersion": "blc.samcday.com/v1alpha1",
                "kind": "AutoScalingGroup",
                "metadata": { "name": name },
                "status": status,
            });
            if let Err(e) = asg_api
                .patch_status(
                    name,
                    &PatchParams::apply("binarylane-controller").force(),
                    &Patch::Apply(&patch),
                )
                .await
            {
                warn!(asg = %name, error = %e, "failed to update ASG status");
            }
        }
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
        let asg = self
            .find_asg(id)
            .ok_or_else(|| Status::not_found(format!("node group {id} not found")))?;
        let count = self.nodes_for_group(&asg).await?.len() as i32;
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
        let asg = self
            .find_asg(&req.id)
            .ok_or_else(|| Status::not_found(format!("node group {} not found", req.id)))?
            .clone();
        let asg_name = asg
            .metadata
            .name
            .clone()
            .ok_or_else(|| Status::invalid_argument("asg missing metadata.name".to_string()))?;
        let current = self.nodes_for_group(&asg).await?;
        if req.delta as usize + current.len() > asg.spec.max_size as usize {
            return Err(Status::invalid_argument(format!(
                "increase would exceed max size {}",
                asg.spec.max_size
            )));
        }

        let password = self.ensure_password(&asg).await?;
        let user_data = if let Some(secret_ref) = &asg.spec.user_data_secret_ref {
            Some(self.read_secret_value(secret_ref, "user-data").await?)
        } else {
            None
        };

        let node_api: Api<K8sNode> = Api::all(self.k8s.clone());
        for i in 0..req.delta {
            let ts = chrono_like_timestamp();
            let name = format!("{}{}-{ts}-{i}", asg.spec.name_prefix, asg_name);

            if let Some(user_data) = &user_data {
                self.create_secret(
                    &controllers::user_data_secret_name(&name),
                    "user-data",
                    user_data,
                )
                .await?;
            }

            self.create_secret(
                &controllers::node_password_secret_name(&name),
                "password",
                &password,
            )
            .await?;

            let mut labels = self.node_labels(&asg_name, &asg.spec);
            labels.insert("kubernetes.io/hostname".to_string(), name.clone());
            labels.insert(controllers::LABEL_SIZE.to_string(), asg.spec.size.clone());
            labels.insert(
                controllers::LABEL_REGION.to_string(),
                asg.spec.region.clone(),
            );
            labels.insert(controllers::LABEL_IMAGE.to_string(), asg.spec.image.clone());

            let node = K8sNode {
                metadata: K8sObjectMeta {
                    name: Some(name.clone()),
                    labels: Some(labels),
                    finalizers: Some(vec![controllers::FINALIZER.to_string()]),
                    ..Default::default()
                },
                spec: Some(K8sNodeSpec {
                    taints: Some(vec![K8sTaint {
                        key: controllers::UNINITIALIZED_TAINT.to_string(),
                        value: Some("true".to_string()),
                        effect: "NoSchedule".to_string(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                status: None,
            };
            match node_api.create(&Default::default(), &node).await {
                Ok(_) => {
                    info!(name = %name, "created k8s node for provisioning");
                }
                Err(kube::Error::Api(err)) if err.code == 409 => {
                    info!(name = %name, "k8s node already exists");
                }
                Err(e) => {
                    return Err(Status::internal(format!("creating k8s node {name}: {e}")));
                }
            }
        }

        let status = AutoScalingGroupStatus {
            last_scale_up: Some(chrono::Utc::now()),
            ..asg.status.clone().unwrap_or_default()
        };
        self.patch_asg_status(&asg_name, status).await?;

        Ok(Response::new(proto::NodeGroupIncreaseSizeResponse {}))
    }

    async fn node_group_delete_nodes(
        &self,
        req: Request<proto::NodeGroupDeleteNodesRequest>,
    ) -> Result<Response<proto::NodeGroupDeleteNodesResponse>, Status> {
        let req = req.into_inner();
        let asg = self
            .find_asg(&req.id)
            .ok_or_else(|| Status::not_found(format!("node group {} not found", req.id)))?;
        let asg_name = asg
            .metadata
            .name
            .clone()
            .ok_or_else(|| Status::invalid_argument("asg missing metadata.name".to_string()))?;
        let nodes_api: Api<K8sNode> = Api::all(self.k8s.clone());
        for node in &req.nodes {
            info!(name = %node.name, "deleting node");
            nodes_api
                .delete(&node.name, &Default::default())
                .await
                .map_err(|e| Status::internal(format!("deleting node {}: {e}", node.name)))?;
        }

        let status = AutoScalingGroupStatus {
            last_scale_down: Some(chrono::Utc::now()),
            ..asg.status.clone().unwrap_or_default()
        };
        self.patch_asg_status(&asg_name, status).await?;

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
        if self.find_asg(id).is_none() {
            return Err(Status::not_found(format!("node group {id} not found")));
        }
        Ok(Response::new(proto::NodeGroupDecreaseTargetSizeResponse {}))
    }

    async fn node_group_nodes(
        &self,
        req: Request<proto::NodeGroupNodesRequest>,
    ) -> Result<Response<proto::NodeGroupNodesResponse>, Status> {
        let id = &req.into_inner().id;
        let asg = self
            .find_asg(id)
            .ok_or_else(|| Status::not_found(format!("node group {id} not found")))?;
        let nodes = self.nodes_for_group(&asg).await?;
        let instances = nodes
            .iter()
            .filter_map(|n| {
                let provider_id = n.spec.as_ref()?.provider_id.as_ref()?;
                Some(proto::Instance {
                    id: provider_id.clone(),
                    status: Some(proto::InstanceStatus {
                        instance_state: proto::instance_status::InstanceState::InstanceRunning
                            as i32,
                        error_info: None,
                    }),
                })
            })
            .collect();
        Ok(Response::new(proto::NodeGroupNodesResponse { instances }))
    }

    async fn node_group_template_node_info(
        &self,
        req: Request<proto::NodeGroupTemplateNodeInfoRequest>,
    ) -> Result<Response<proto::NodeGroupTemplateNodeInfoResponse>, Status> {
        let id = &req.into_inner().id;
        let asg = self
            .find_asg(id)
            .ok_or_else(|| Status::not_found(format!("node group {id} not found")))?;
        let asg_name = asg
            .metadata
            .name
            .clone()
            .ok_or_else(|| Status::invalid_argument("asg missing metadata.name".to_string()))?;

        let size_info = if asg.spec.vcpus.is_some()
            && asg.spec.memory_mb.is_some()
            && asg.spec.disk_gb.is_some()
        {
            None
        } else {
            let sizes = self.get_sizes().await?;
            Some(
                sizes
                    .into_iter()
                    .find(|s| s.slug == asg.spec.size)
                    .ok_or_else(|| {
                        Status::not_found(format!("BinaryLane size {} not found", asg.spec.size))
                    })?,
            )
        };

        let vcpus = asg
            .spec
            .vcpus
            .or_else(|| size_info.as_ref().map(|s| s.vcpus))
            .ok_or_else(|| Status::internal("missing vcpus for template node info".to_string()))?;
        let memory_mb = asg
            .spec
            .memory_mb
            .or_else(|| size_info.as_ref().map(|s| s.memory))
            .ok_or_else(|| Status::internal("missing memory for template node info".to_string()))?;
        let disk_gb = asg
            .spec
            .disk_gb
            .or_else(|| size_info.as_ref().map(|s| s.disk))
            .ok_or_else(|| Status::internal("missing disk for template node info".to_string()))?;

        let mut labels = self.node_labels(&asg_name, &asg.spec);
        labels.insert(
            "kubernetes.io/hostname".to_string(),
            format!("template-{asg_name}"),
        );

        let mut capacity = BTreeMap::new();
        capacity.insert(
            "cpu".to_string(),
            Quantity {
                string: Some(vcpus.to_string()),
            },
        );
        capacity.insert(
            "memory".to_string(),
            Quantity {
                string: Some(format!("{}Mi", memory_mb)),
            },
        );
        capacity.insert(
            "ephemeral-storage".to_string(),
            Quantity {
                string: Some(format!("{}Gi", disk_gb)),
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
                string: Some(format!("{}m", (vcpus * 1000).saturating_sub(100))),
            },
        );
        allocatable.insert(
            "memory".to_string(),
            Quantity {
                string: Some(format!("{}Mi", memory_mb.saturating_sub(256))),
            },
        );
        allocatable.insert(
            "ephemeral-storage".to_string(),
            Quantity {
                string: Some(format!("{}Gi", disk_gb.saturating_sub(1))),
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
                name: Some(format!("template-{asg_name}")),
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

fn chrono_like_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    // Millisecond granularity avoids name collisions from rapid successive calls
    format!("{}", dur.as_millis())
}
