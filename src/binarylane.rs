use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

const DEFAULT_API_BASE: &str = "https://api.binarylane.com.au/v2";

pub const PROVIDER_NAME: &str = "binarylane";

pub fn server_provider_id(server_id: i64) -> String {
    format!("{PROVIDER_NAME}:///{server_id}")
}

pub fn parse_provider_id(pid: &str) -> Option<i64> {
    let prefix = format!("{PROVIDER_NAME}:///");
    pid.strip_prefix(&prefix)?.parse().ok()
}

#[derive(Clone)]
pub struct Client {
    token: String,
    api_base: String,
    http: reqwest::Client,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Server {
    pub id: i64,
    pub name: String,
    pub status: String,
    pub size_slug: String,
    pub region: Region,
    #[allow(dead_code)]
    pub image: Image,
    pub networks: Networks,
    #[allow(dead_code)]
    pub vpc_id: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Region {
    pub slug: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Image {
    #[allow(dead_code)]
    pub slug: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListedImage {
    #[allow(dead_code)]
    pub id: i64,
    pub slug: Option<String>,
    #[allow(dead_code)]
    pub name: String,
    #[allow(dead_code)]
    pub distribution: String,
    #[allow(dead_code)]
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Networks {
    pub v4: Vec<NetworkV4>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NetworkV4 {
    pub ip_address: String,
    #[serde(rename = "type")]
    pub net_type: String,
}

#[derive(Debug, Serialize)]
pub struct CreateServerRequest {
    pub name: String,
    pub size: String,
    pub image: String,
    pub region: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_data: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_keys: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoadBalancer {
    pub id: i64,
    pub name: String,
    pub ip: String,
    #[allow(dead_code)]
    pub status: String,
    pub forwarding_rules: Vec<ForwardingRule>,
    pub server_ids: Vec<i64>,
    #[allow(dead_code)]
    pub region: Region,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForwardingRule {
    pub entry_protocol: String,
    pub entry_port: i32,
    pub target_protocol: String,
    pub target_port: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheck {
    pub protocol: String,
    pub port: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub check_interval_seconds: i32,
    pub response_timeout_seconds: i32,
    pub unhealthy_threshold: i32,
    pub healthy_threshold: i32,
}

#[derive(Debug, Serialize)]
pub struct CreateLoadBalancerRequest {
    pub name: String,
    pub region: String,
    pub forwarding_rules: Vec<ForwardingRule>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_check: Option<HealthCheck>,
    pub server_ids: Vec<i64>,
}

#[derive(Debug, Serialize)]
pub struct UpdateLoadBalancerRequest {
    pub name: String,
    pub forwarding_rules: Vec<ForwardingRule>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_check: Option<HealthCheck>,
    pub server_ids: Vec<i64>,
}

impl Client {
    pub fn new(token: String) -> Self {
        Self {
            token,
            api_base: DEFAULT_API_BASE.to_string(),
            http: reqwest::Client::new(),
        }
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.api_base))
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
    }

    pub async fn get_server(&self, server_id: i64) -> Result<Option<Server>> {
        let resp = self
            .request(reqwest::Method::GET, &format!("/servers/{server_id}"))
            .send()
            .await
            .context("getting server")?;
        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("getting server {server_id}: {status}: {body}");
        }
        #[derive(Deserialize)]
        struct Resp {
            server: Server,
        }
        let r: Resp = resp.json().await.context("decoding server")?;
        Ok(Some(r.server))
    }

    pub async fn list_servers(&self) -> Result<Vec<Server>> {
        let mut all = Vec::new();
        let mut page = 1u32;
        loop {
            let resp = self
                .request(
                    reqwest::Method::GET,
                    &format!("/servers?page={page}&per_page=100"),
                )
                .send()
                .await
                .context("listing servers")?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                bail!("listing servers: {status}: {body}");
            }
            #[derive(Deserialize)]
            struct Pages {
                next: Option<String>,
            }
            #[derive(Deserialize)]
            struct Links {
                pages: Pages,
            }
            #[derive(Deserialize)]
            struct Resp {
                servers: Vec<Server>,
                links: Option<Links>,
            }
            let r: Resp = resp.json().await.context("decoding servers")?;
            all.extend(r.servers);
            if r.links.and_then(|l| l.pages.next).is_none() {
                break;
            }
            page += 1;
        }
        Ok(all)
    }

    pub async fn list_images(&self) -> Result<Vec<ListedImage>> {
        let mut all = Vec::new();
        let mut page = 1u32;
        loop {
            let resp = self
                .request(
                    reqwest::Method::GET,
                    &format!("/images?page={page}&per_page=100"),
                )
                .send()
                .await
                .context("listing images")?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                bail!("listing images: {status}: {body}");
            }
            #[derive(Deserialize)]
            struct Pages {
                next: Option<String>,
            }
            #[derive(Deserialize)]
            struct Links {
                pages: Pages,
            }
            #[derive(Deserialize)]
            struct Resp {
                images: Vec<ListedImage>,
                links: Option<Links>,
            }
            let r: Resp = resp.json().await.context("decoding images")?;
            all.extend(r.images);
            if r.links.and_then(|l| l.pages.next).is_none() {
                break;
            }
            page += 1;
        }
        Ok(all)
    }

    pub async fn create_server(&self, req: CreateServerRequest) -> Result<Server> {
        let resp = self
            .request(reqwest::Method::POST, "/servers")
            .json(&req)
            .send()
            .await
            .context("creating server")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("creating server: {status}: {body}");
        }
        #[derive(Deserialize)]
        struct Resp {
            server: Server,
        }
        let r: Resp = resp.json().await.context("decoding server")?;
        Ok(r.server)
    }

    pub async fn delete_server(&self, server_id: i64) -> Result<()> {
        let resp = self
            .request(reqwest::Method::DELETE, &format!("/servers/{server_id}"))
            .send()
            .await
            .context("deleting server")?;
        if resp.status().as_u16() == 404 {
            return Ok(());
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("deleting server {server_id}: {status}: {body}");
        }
        Ok(())
    }

    pub async fn get_load_balancer(&self, lb_id: i64) -> Result<Option<LoadBalancer>> {
        let resp = self
            .request(reqwest::Method::GET, &format!("/load_balancers/{lb_id}"))
            .send()
            .await
            .context("getting load balancer")?;
        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("getting load balancer {lb_id}: {status}: {body}");
        }
        #[derive(Deserialize)]
        struct Resp {
            load_balancer: LoadBalancer,
        }
        let r: Resp = resp.json().await.context("decoding load balancer")?;
        Ok(Some(r.load_balancer))
    }

    pub async fn create_load_balancer(
        &self,
        req: CreateLoadBalancerRequest,
    ) -> Result<LoadBalancer> {
        let resp = self
            .request(reqwest::Method::POST, "/load_balancers")
            .json(&req)
            .send()
            .await
            .context("creating load balancer")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("creating load balancer: {status}: {body}");
        }
        #[derive(Deserialize)]
        struct Resp {
            load_balancer: LoadBalancer,
        }
        let r: Resp = resp.json().await.context("decoding load balancer")?;
        Ok(r.load_balancer)
    }

    pub async fn update_load_balancer(
        &self,
        lb_id: i64,
        req: UpdateLoadBalancerRequest,
    ) -> Result<LoadBalancer> {
        let resp = self
            .request(reqwest::Method::PUT, &format!("/load_balancers/{lb_id}"))
            .json(&req)
            .send()
            .await
            .context("updating load balancer")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("updating load balancer {lb_id}: {status}: {body}");
        }
        #[derive(Deserialize)]
        struct Resp {
            load_balancer: LoadBalancer,
        }
        let r: Resp = resp.json().await.context("decoding load balancer")?;
        Ok(r.load_balancer)
    }

    pub async fn delete_load_balancer(&self, lb_id: i64) -> Result<()> {
        let resp = self
            .request(reqwest::Method::DELETE, &format!("/load_balancers/{lb_id}"))
            .send()
            .await
            .context("deleting load balancer")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("deleting load balancer {lb_id}: {status}: {body}");
        }
        Ok(())
    }
}
