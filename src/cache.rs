use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::sync::Mutex;

#[derive(Clone, Default)]
pub struct AppCache {
    local: Arc<Mutex<HashMap<String, (String, Instant)>>>,
    locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl AppCache {
    pub fn new() -> Self {
        Self {
            local: Default::default(),
            locks: Default::default(),
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
        local.insert(key.into(), (value, Instant::now() + ttl));
    }

    pub async fn lock(&self, key: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.locks.lock().await;
            locks
                .entry(key.into())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        lock.lock_owned().await
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
