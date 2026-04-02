use anyhow::{Context, Result, bail};
use rand::Rng;
use rand::distributions::Alphanumeric;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};

const DEFAULT_API_BASE: &str = "https://api.binarylane.com.au/v2";

pub const PROVIDER_NAME: &str = "binarylane";

pub fn server_provider_id(server_id: i64) -> String {
    format!("{PROVIDER_NAME}:///{server_id}")
}

/// Generate a cryptographically secure random password for BinaryLane server creation.
/// Uses OS-provided entropy via `OsRng`. The "BlcA1" prefix guarantees the password
/// meets typical complexity requirements (uppercase, lowercase, digit).
pub fn generate_server_password() -> String {
    let suffix: String = OsRng
        .sample_iter(Alphanumeric)
        .take(36)
        .map(char::from)
        .collect();
    format!("BlcA1{suffix}")
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
    pub image: Image,
    pub networks: Networks,
    pub vpc_id: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Region {
    pub slug: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Image {
    pub slug: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListedImage {
    pub id: i64,
    pub slug: Option<String>,
    pub name: String,
    pub distribution: String,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoadBalancer {
    pub id: i64,
    pub name: String,
    pub ip: String,
    pub status: String,
    pub forwarding_rules: Vec<ForwardingRule>,
    pub server_ids: Vec<i64>,
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

#[derive(Debug, Clone, Deserialize)]
pub struct Domain {
    pub id: i64,
    pub name: String,
    pub ttl: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DomainRecord {
    pub id: i64,
    #[serde(rename = "type")]
    pub record_type: String,
    pub name: String,
    pub data: String,
    pub priority: Option<i64>,
    pub port: Option<i64>,
    pub weight: Option<i64>,
    pub ttl: i64,
}

#[derive(Debug, Serialize)]
pub struct CreateDomainRecordRequest {
    #[serde(rename = "type")]
    pub record_type: String,
    pub name: String,
    pub data: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight: Option<i64>,
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

    pub async fn get_server_by_hostname(&self, hostname: &str) -> Result<Option<Server>> {
        let resp = self
            .request(
                reqwest::Method::GET,
                &format!("/servers?hostname={hostname}"),
            )
            .send()
            .await
            .context("getting server by hostname")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("getting server by hostname {hostname}: {status}: {body}");
        }
        #[derive(Deserialize)]
        struct Resp {
            servers: Vec<Server>,
        }
        let r: Resp = resp.json().await.context("decoding server")?;
        Ok(r.servers.into_iter().next())
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

    pub async fn list_domains(&self) -> Result<Vec<Domain>> {
        let mut all = Vec::new();
        let mut page = 1u32;
        loop {
            let resp = self
                .request(
                    reqwest::Method::GET,
                    &format!("/domains?page={page}&per_page=100"),
                )
                .send()
                .await
                .context("listing domains")?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                bail!("listing domains: {status}: {body}");
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
                domains: Vec<Domain>,
                links: Option<Links>,
            }
            let r: Resp = resp.json().await.context("decoding domains")?;
            all.extend(r.domains);
            if r.links.and_then(|l| l.pages.next).is_none() {
                break;
            }
            page += 1;
        }
        Ok(all)
    }

    pub async fn list_domain_records(&self, domain: &str) -> Result<Vec<DomainRecord>> {
        let mut all = Vec::new();
        let mut page = 1u32;
        loop {
            let resp = self
                .request(
                    reqwest::Method::GET,
                    &format!("/domains/{domain}/records?page={page}&per_page=100"),
                )
                .send()
                .await
                .context("listing domain records")?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                bail!("listing domain records for {domain}: {status}: {body}");
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
                domain_records: Vec<DomainRecord>,
                links: Option<Links>,
            }
            let r: Resp = resp.json().await.context("decoding domain records")?;
            all.extend(r.domain_records);
            if r.links.and_then(|l| l.pages.next).is_none() {
                break;
            }
            page += 1;
        }
        Ok(all)
    }

    pub async fn create_domain_record(
        &self,
        domain: &str,
        req: CreateDomainRecordRequest,
    ) -> Result<DomainRecord> {
        let resp = self
            .request(reqwest::Method::POST, &format!("/domains/{domain}/records"))
            .json(&req)
            .send()
            .await
            .context("creating domain record")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("creating domain record in {domain}: {status}: {body}");
        }
        #[derive(Deserialize)]
        struct Resp {
            domain_record: DomainRecord,
        }
        let r: Resp = resp.json().await.context("decoding domain record")?;
        Ok(r.domain_record)
    }

    pub async fn delete_domain_record(&self, domain: &str, record_id: i64) -> Result<()> {
        let resp = self
            .request(
                reqwest::Method::DELETE,
                &format!("/domains/{domain}/records/{record_id}"),
            )
            .send()
            .await
            .context("deleting domain record")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("deleting domain record {record_id} in {domain}: {status}: {body}");
        }
        Ok(())
    }

    pub async fn refresh_nameserver_cache(&self, domain_names: &[&str]) -> Result<()> {
        #[derive(Serialize)]
        struct Req<'a> {
            domain_names: &'a [&'a str],
        }
        let resp = self
            .request(reqwest::Method::POST, "/domains/refresh_nameserver_cache")
            .json(&Req { domain_names })
            .send()
            .await
            .context("refreshing nameserver cache")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("refreshing nameserver cache: {status}: {body}");
        }
        Ok(())
    }
}
