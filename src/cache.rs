use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::sync::Mutex;

#[derive(Clone, Default)]
pub struct AppCache {
    local: Arc<Mutex<HashMap<String, (String, Instant)>>>,
}

impl AppCache {
    pub fn new() -> Self {
        Self {
            local: Default::default(),
        }
    }

    pub async fn get(&self, key: &str) -> Option<String> {
        let mut local = self.local.lock().await;
        match local.get(key) {
            Some((value, expiry)) if *expiry > Instant::now() => Some(value.clone()),
            Some(_) => {
                local.remove(key);
                None
            }
            None => None,
        }
    }

    pub async fn set(&self, key: &str, value: String, ttl: Duration) {
        let mut local = self.local.lock().await;
        // 过期条目只在同 key 读取时清除，写入前顺手回收，避免长期运行内存增长。
        if local.len() >= 512 {
            let now = Instant::now();
            local.retain(|_, (_, expiry)| *expiry > now);
        }
        local.insert(key.into(), (value, Instant::now() + ttl));
    }

    pub async fn allow(&self, subject: &str, count: u64, ttl: Duration) -> bool {
        if count == 0 {
            return true;
        }
        let key = format!("rate:{subject}");
        let now = Instant::now();
        let mut local = self.local.lock().await;
        let (mut n, expiry) = local
            .get(&key)
            .and_then(|(value, expiry)| value.parse().ok().map(|n| (n, *expiry)))
            .filter(|(_, expiry)| *expiry > now)
            .unwrap_or((0_u64, now + ttl));
        n += 1;
        local.insert(key, (n.to_string(), expiry));
        n <= count
    }
}
