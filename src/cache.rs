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
    #[cfg(feature = "redis-cache")]
    redis: Option<redis::aio::ConnectionManager>,
}

impl AppCache {
    pub async fn new(redis_url: Option<&str>) -> Self {
        #[cfg(not(feature = "redis-cache"))]
        let _ = redis_url;
        #[cfg(feature = "redis-cache")]
        let redis = if let Some(url) = redis_url {
            match redis::Client::open(url) {
                Ok(client) => redis::aio::ConnectionManager::new(client).await.ok(),
                Err(_) => None,
            }
        } else {
            None
        };
        Self {
            local: Default::default(),
            locks: Default::default(),
            #[cfg(feature = "redis-cache")]
            redis,
        }
    }
    pub async fn get(&self, key: &str) -> Option<String> {
        #[cfg(feature = "redis-cache")]
        if let Some(mut conn) = self.redis.clone() {
            use redis::AsyncCommands;
            if let Ok(value) = conn.get::<_, Option<String>>(key).await
                && value.is_some()
            {
                return value;
            }
        }
        let mut local = self.local.lock().await;
        match local.get(key) {
            Some((v, expiry)) if *expiry > Instant::now() => Some(v.clone()),
            Some(_) => {
                local.remove(key);
                None
            }
            None => None,
        }
    }
    pub async fn set(&self, key: &str, value: String, ttl: Duration) {
        #[cfg(feature = "redis-cache")]
        if let Some(mut conn) = self.redis.clone() {
            use redis::AsyncCommands;
            let _: redis::RedisResult<()> = conn.set_ex(key, &value, ttl.as_secs()).await;
        }
        self.local
            .lock()
            .await
            .insert(key.into(), (value, Instant::now() + ttl));
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
        #[cfg(feature = "redis-cache")]
        if let Some(mut conn) = self.redis.clone() {
            use redis::AsyncCommands;
            if let Ok(n) = conn.incr::<_, _, u64>(&key, 1).await {
                if n == 1 {
                    let _: redis::RedisResult<bool> = conn.expire(&key, ttl.as_secs() as i64).await;
                }
                return n <= count;
            }
        }
        let now = Instant::now();
        let mut local = self.local.lock().await;
        let (mut n, expiry) = local
            .get(&key)
            .and_then(|(v, e)| v.parse().ok().map(|n| (n, *e)))
            .filter(|(_, e)| *e > now)
            .unwrap_or((0_u64, now + ttl));
        n += 1;
        local.insert(key, (n.to_string(), expiry));
        n <= count
    }
}
