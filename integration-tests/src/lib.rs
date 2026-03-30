use std::future::Future;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};

pub struct TestContext {
    pub bl: binarylane_client::Client,
    pub k8s: kube::Client,
}

impl TestContext {
    pub async fn new() -> Option<Self> {
        let token = std::env::var("BL_API_TOKEN").ok()?;
        std::env::var("KUBECONFIG").ok()?;
        let k8s = kube::Client::try_default().await.ok()?;
        Some(Self {
            bl: binarylane_client::Client::new(token),
            k8s,
        })
    }
}

pub async fn wait_for<F, Fut>(timeout: Duration, interval: Duration, condition: F) -> Result<()>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<bool>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if condition().await? {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("timed out after {timeout:?}");
        }
        tokio::time::sleep(interval).await;
    }
}

pub fn test_name(base: &str) -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    format!("blctest-{base}-{ts}")
}
