use std::path::Path;

use anyhow::{Context, Result};
use sqlx::{Row, SqlitePool, sqlite::SqliteConnectOptions};

#[derive(Clone)]
pub struct Storage {
    pool: SqlitePool,
}

impl Storage {
    pub async fn connect(database_url: &str) -> Result<Self> {
        if let Some(path) = database_url
            .strip_prefix("sqlite://")
            .and_then(|p| p.split('?').next())
            && let Some(parent) = Path::new(path).parent()
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        let options: SqliteConnectOptions = database_url
            .parse()
            .context("DATABASE_URL 不是有效的 SQLite URL")?;
        let pool = SqlitePool::connect_with(options.create_if_missing(true)).await?;
        sqlx::migrate!().run(&pool).await?;
        Ok(Self { pool })
    }
    pub async fn get_file_id(&self, cache_key: &str, media_kind: &str) -> Result<Option<String>> {
        let row = sqlx::query(
            "SELECT file_id FROM telegram_media_cache WHERE cache_key=? AND media_kind=?",
        )
        .bind(cache_key)
        .bind(media_kind)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.get("file_id")))
    }
    pub async fn put_file_id(
        &self,
        cache_key: &str,
        media_kind: &str,
        file_id: &str,
        unique_id: Option<&str>,
    ) -> Result<()> {
        sqlx::query("INSERT INTO telegram_media_cache(cache_key,media_kind,file_id,file_unique_id) VALUES(?,?,?,?) ON CONFLICT(cache_key,media_kind) DO UPDATE SET file_id=excluded.file_id,file_unique_id=excluded.file_unique_id,updated_at=unixepoch()")
            .bind(cache_key).bind(media_kind).bind(file_id).bind(unique_id).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn get_credential(&self, provider: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT value FROM provider_credentials WHERE provider=?")
            .bind(provider)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get("value")))
    }

    pub async fn put_credential(&self, provider: &str, value: &str) -> Result<()> {
        sqlx::query("INSERT INTO provider_credentials(provider,value) VALUES(?,?) ON CONFLICT(provider) DO UPDATE SET value=excluded.value,updated_at=unixepoch()")
            .bind(provider)
            .bind(value)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
