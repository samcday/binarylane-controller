use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Cursor};
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rustls::RootCertStore;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tracing::error;

use crate::binarylane;

const WEBHOOK_CONTENT_TYPE: &str = "application/external.dns.webhook+json;version=1";

#[derive(Clone)]
pub struct TlsConfig {
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
    pub ca_pem: Vec<u8>,
}

#[derive(Clone)]
struct AppState {
    bl: binarylane::Client,
}

#[derive(Debug)]
struct AppError(anyhow::Error);

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DomainFilterResponse {
    domain_filter: DomainFilter,
}

#[derive(Debug, Serialize)]
struct DomainFilter {
    include: Vec<String>,
    exclude: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Endpoint {
    dns_name: String,
    targets: Vec<String>,
    record_type: String,
    #[serde(rename = "recordTTL")]
    record_ttl: i64,
    labels: HashMap<String, String>,
    provider_specific: Vec<ProviderSpecificProperty>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderSpecificProperty {
    name: String,
    value: String,
}

#[derive(Debug, Deserialize)]
struct Changes {
    #[serde(rename = "Create", default)]
    create: Vec<Endpoint>,
    #[serde(rename = "UpdateOld", default)]
    update_old: Vec<Endpoint>,
    #[serde(rename = "UpdateNew", default)]
    update_new: Vec<Endpoint>,
    #[serde(rename = "Delete", default)]
    delete: Vec<Endpoint>,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

struct WebhookJson<T>(T);

impl<T: Serialize> IntoResponse for WebhookJson<T> {
    fn into_response(self) -> Response {
        let mut response = Json(self.0).into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(WEBHOOK_CONTENT_TYPE),
        );
        response
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let mut response = (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: self.0.to_string(),
            }),
        )
            .into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(WEBHOOK_CONTENT_TYPE),
        );
        response
    }
}

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(value: E) -> Self {
        Self(value.into())
    }
}

pub async fn run(bl: binarylane::Client, addr: SocketAddr, tls: Option<TlsConfig>) -> Result<()> {
    let state = AppState { bl };
    let app = Router::new()
        .route("/", get(get_root))
        .route("/records", get(get_records).post(post_records))
        .route("/adjustendpoints", post(post_adjust_endpoints))
        .with_state(state);

    if let Some(tls) = tls {
        let listener = TcpListener::bind(addr)
            .await
            .context("binding DNS webhook listener")?;
        let acceptor = TlsAcceptor::from(Arc::new(build_tls_config(tls)?));
        let listener = RustlsListener { listener, acceptor };
        axum::serve(listener, app)
            .await
            .context("serving DNS webhook over mTLS")?;
    } else {
        let listener = TcpListener::bind(addr)
            .await
            .context("binding DNS webhook listener")?;
        axum::serve(listener, app)
            .await
            .context("serving DNS webhook")?;
    }

    Ok(())
}

async fn get_root(
    State(state): State<AppState>,
) -> std::result::Result<WebhookJson<DomainFilterResponse>, AppError> {
    let domains = state.bl.list_domains().await?;
    Ok(WebhookJson(DomainFilterResponse {
        domain_filter: DomainFilter {
            include: domains.into_iter().map(|d| d.name).collect(),
            exclude: Vec::new(),
        },
    }))
}

async fn get_records(
    State(state): State<AppState>,
) -> std::result::Result<WebhookJson<Vec<Endpoint>>, AppError> {
    let domains = state.bl.list_domains().await?;
    let mut grouped: BTreeMap<(String, String), Endpoint> = BTreeMap::new();

    for domain in domains {
        let records = state.bl.list_domain_records(&domain.name).await?;
        for record in records {
            if !supported_record_type(&record.record_type) {
                continue;
            }

            let dns_name = record_dns_name(&domain.name, &record.name);
            let key = (dns_name.clone(), record.record_type.clone());
            let endpoint = grouped.entry(key).or_insert_with(|| Endpoint {
                dns_name,
                targets: Vec::new(),
                record_type: record.record_type.clone(),
                record_ttl: record.ttl,
                labels: HashMap::new(),
                provider_specific: Vec::new(),
            });
            endpoint.targets.push(record.data);
        }
    }

    let mut endpoints: Vec<Endpoint> = grouped.into_values().collect();
    for endpoint in &mut endpoints {
        endpoint.targets.sort();
        endpoint.targets.dedup();
    }

    Ok(WebhookJson(endpoints))
}

async fn post_records(
    State(state): State<AppState>,
    Json(changes): Json<Changes>,
) -> std::result::Result<StatusCode, AppError> {
    let domains = state.bl.list_domains().await?;
    let domain_names: Vec<String> = domains.iter().map(|d| d.name.clone()).collect();

    for endpoint in &changes.create {
        let (domain, record_name) = map_endpoint_to_domain(endpoint, &domain_names)?;
        let priority = endpoint_priority(endpoint)?;
        let port = endpoint_port(endpoint)?;
        let weight = endpoint_weight(endpoint)?;
        for target in &endpoint.targets {
            state
                .bl
                .create_domain_record(
                    &domain,
                    binarylane::CreateDomainRecordRequest {
                        record_type: endpoint.record_type.clone(),
                        name: record_name.clone(),
                        data: target.clone(),
                        priority,
                        port,
                        weight,
                    },
                )
                .await?;
        }
    }

    apply_updates(
        &state,
        &domain_names,
        &changes.update_old,
        &changes.update_new,
    )
    .await?;
    apply_deletes(&state, &domain_names, &changes.delete).await?;

    Ok(StatusCode::NO_CONTENT)
}

async fn post_adjust_endpoints(
    Json(endpoints): Json<Vec<Endpoint>>,
) -> std::result::Result<WebhookJson<Vec<Endpoint>>, AppError> {
    Ok(WebhookJson(endpoints))
}

async fn apply_updates(
    state: &AppState,
    domains: &[String],
    old: &[Endpoint],
    new: &[Endpoint],
) -> Result<()> {
    for endpoint in old {
        let (domain, record_name) = map_endpoint_to_domain(endpoint, domains)?;
        let records = state.bl.list_domain_records(&domain).await?;
        for record in records {
            if record.name == record_name
                && record
                    .record_type
                    .eq_ignore_ascii_case(&endpoint.record_type)
            {
                state.bl.delete_domain_record(&domain, record.id).await?;
            }
        }
    }

    for endpoint in new {
        let (domain, record_name) = map_endpoint_to_domain(endpoint, domains)?;
        let priority = endpoint_priority(endpoint)?;
        let port = endpoint_port(endpoint)?;
        let weight = endpoint_weight(endpoint)?;
        for target in &endpoint.targets {
            state
                .bl
                .create_domain_record(
                    &domain,
                    binarylane::CreateDomainRecordRequest {
                        record_type: endpoint.record_type.clone(),
                        name: record_name.clone(),
                        data: target.clone(),
                        priority,
                        port,
                        weight,
                    },
                )
                .await?;
        }
    }

    Ok(())
}

async fn apply_deletes(state: &AppState, domains: &[String], endpoints: &[Endpoint]) -> Result<()> {
    for endpoint in endpoints {
        let (domain, record_name) = map_endpoint_to_domain(endpoint, domains)?;
        let targets: HashSet<&str> = endpoint.targets.iter().map(String::as_str).collect();
        let records = state.bl.list_domain_records(&domain).await?;
        for record in records {
            if record.name != record_name
                || !record
                    .record_type
                    .eq_ignore_ascii_case(&endpoint.record_type)
            {
                continue;
            }

            if targets.is_empty() || targets.contains(record.data.as_str()) {
                state.bl.delete_domain_record(&domain, record.id).await?;
            }
        }
    }
    Ok(())
}

fn supported_record_type(record_type: &str) -> bool {
    matches!(record_type, "A" | "AAAA" | "CNAME" | "TXT" | "SRV" | "NS")
}

fn normalize_dns_name(name: &str) -> String {
    name.trim_end_matches('.').to_ascii_lowercase()
}

fn record_dns_name(domain: &str, record_name: &str) -> String {
    if record_name == "@" {
        domain.to_string()
    } else {
        format!("{record_name}.{domain}")
    }
}

fn map_endpoint_to_domain(endpoint: &Endpoint, domains: &[String]) -> Result<(String, String)> {
    let dns_name = normalize_dns_name(&endpoint.dns_name);

    let mut best: Option<&String> = None;
    for domain in domains {
        let candidate = normalize_dns_name(domain);
        if (dns_name == candidate || dns_name.ends_with(&format!(".{candidate}")))
            && best
                .as_ref()
                .is_none_or(|current| candidate.len() > current.len())
        {
            best = Some(domain);
        }
    }

    let Some(domain) = best else {
        bail!("no matching domain for endpoint {}", endpoint.dns_name);
    };

    let normalized_domain = normalize_dns_name(domain);
    let record_name = if dns_name == normalized_domain {
        "@".to_string()
    } else {
        dns_name
            .strip_suffix(&format!(".{normalized_domain}"))
            .unwrap_or_default()
            .to_string()
    };

    Ok((domain.clone(), record_name))
}

fn endpoint_priority(endpoint: &Endpoint) -> Result<Option<i64>> {
    parse_provider_specific_i64(endpoint, "priority")
}

fn endpoint_port(endpoint: &Endpoint) -> Result<Option<i64>> {
    parse_provider_specific_i64(endpoint, "port")
}

fn endpoint_weight(endpoint: &Endpoint) -> Result<Option<i64>> {
    parse_provider_specific_i64(endpoint, "weight")
}

fn parse_provider_specific_i64(endpoint: &Endpoint, key: &str) -> Result<Option<i64>> {
    let value = endpoint
        .provider_specific
        .iter()
        .find(|entry| entry.name.eq_ignore_ascii_case(key))
        .map(|entry| entry.value.as_str());

    match value {
        Some(v) => Ok(Some(v.parse::<i64>().with_context(|| {
            format!("parsing provider specific value {key}={v}")
        })?)),
        None => Ok(None),
    }
}

fn build_tls_config(tls: TlsConfig) -> Result<rustls::ServerConfig> {
    let certs = read_certs(&tls.cert_pem).context("decoding TLS cert")?;
    let key = read_private_key(&tls.key_pem).context("decoding TLS key")?;
    let client_certs = read_certs(&tls.ca_pem).context("decoding TLS CA cert")?;

    let mut roots = RootCertStore::empty();
    roots.add_parsable_certificates(client_certs);

    let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .context("building client cert verifier")?;

    rustls::ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .context("building TLS server config")
}

fn read_certs(pem: &[u8]) -> io::Result<Vec<CertificateDer<'static>>> {
    rustls_pemfile::certs(&mut Cursor::new(pem)).collect()
}

fn read_private_key(pem: &[u8]) -> Result<PrivateKeyDer<'static>> {
    let mut reader = Cursor::new(pem);
    if let Some(key) = rustls_pemfile::pkcs8_private_keys(&mut reader)
        .next()
        .transpose()
        .context("reading PKCS8 private key")?
    {
        return Ok(PrivateKeyDer::from(key));
    }

    let mut reader = Cursor::new(pem);
    if let Some(key) = rustls_pemfile::rsa_private_keys(&mut reader)
        .next()
        .transpose()
        .context("reading RSA private key")?
    {
        return Ok(PrivateKeyDer::from(key));
    }

    bail!("no TLS private key found")
}

struct RustlsListener {
    listener: TcpListener,
    acceptor: TlsAcceptor,
}

impl axum::serve::Listener for RustlsListener {
    type Io = tokio_rustls::server::TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.listener.accept().await {
                Ok((stream, addr)) => match self.acceptor.accept(stream).await {
                    Ok(tls_stream) => return (tls_stream, addr),
                    Err(error) => error!(error = %error, "TLS handshake failed"),
                },
                Err(error) => error!(error = %error, "accepting TCP connection failed"),
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}
