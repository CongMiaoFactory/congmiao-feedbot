use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use tokio::sync::RwLock;

use crate::{Config, storage::Storage};

#[derive(Clone)]
pub struct RuntimeCredentials {
    storage: Option<Storage>,
    bilibili: Arc<RwLock<Option<String>>>,
    netease: Arc<RwLock<Option<String>>>,
    revision: Arc<AtomicU64>,
}

impl RuntimeCredentials {
    pub fn memory(config: &Config) -> Self {
        Self {
            storage: None,
            bilibili: Arc::new(RwLock::new(config.bilibili_cookie.clone())),
            netease: Arc::new(RwLock::new(config.netease_cookie.clone())),
            revision: Arc::new(AtomicU64::new(fresh_revision())),
        }
    }

    pub async fn load(storage: Storage, config: &Config) -> Result<Self> {
        let bilibili = storage
            .get_credential("bilibili")
            .await?
            .or_else(|| config.bilibili_cookie.clone());
        let netease = storage
            .get_credential("netease")
            .await?
            .or_else(|| config.netease_cookie.clone());
        Ok(Self {
            storage: Some(storage),
            bilibili: Arc::new(RwLock::new(bilibili)),
            netease: Arc::new(RwLock::new(netease)),
            revision: Arc::new(AtomicU64::new(fresh_revision())),
        })
    }

    pub async fn bilibili(&self) -> Option<String> {
        self.bilibili.read().await.clone()
    }

    pub async fn netease(&self) -> Option<String> {
        self.netease.read().await.clone()
    }

    pub async fn set_bilibili(&self, cookie: String) -> Result<()> {
        if let Some(storage) = &self.storage {
            storage.put_credential("bilibili", &cookie).await?;
        }
        *self.bilibili.write().await = Some(cookie);
        self.revision.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub async fn set_netease(&self, cookie: String) -> Result<()> {
        if let Some(storage) = &self.storage {
            storage.put_credential("netease", &cookie).await?;
        }
        *self.netease.write().await = Some(cookie);
        self.revision.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Relaxed)
    }
}

fn fresh_revision() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}
