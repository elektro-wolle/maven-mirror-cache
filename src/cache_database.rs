use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_derive::Serialize;
use sqlx::{PgPool, Row};
use std::collections::HashMap;
use std::error::Error;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::mpsc::Sender;
use tokio::time::interval;
use tracing::{debug, info, warn};

// Database models
#[allow(dead_code)]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CacheEntry {
    pub id: i64,
    pub artifact_path: String,
    pub file_path: Option<String>,
    pub size_bytes: Option<i64>,
    pub downloads: i64,
    pub content_type: String,
    pub source_repository: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_accessed: DateTime<Utc>,
    pub last_modified: DateTime<Utc>,
    pub is_negative_cache: bool,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Clone)]
pub struct CacheDatabase {
    pub db_pool: PgPool,
    tracker: DownloadTracker,
}

#[derive(Debug, Serialize)]
pub struct CacheStats {
    pub total_entries: i64,
    pub negative_entries: i64,
    pub positive_entries: i64,
    pub total_size_bytes: i64,
}

#[derive(Clone)]
pub struct DownloadTracker {
    sender: Sender<String>,
}

#[async_trait]
pub trait FileDeleter {
    async fn delete_file(&self, path: &PathBuf) -> Result<(), Box<dyn Error>>;
}

/// Performs batched updates to the DB
#[async_trait]
pub trait CacheDBUpdateCounter {
    async fn update_counts(
        &self,
        pool: &PgPool,
        counts: &HashMap<String, i32>,
    ) -> Result<(), sqlx::Error>;
}

#[async_trait]
pub trait CacheDBAccess {
    async fn get_cache_entry(&self, artifact_path: &str)
    -> Result<Option<CacheEntry>, sqlx::Error>;

    async fn update_last_accessed(&self, artifact_path: &str) -> Result<(), sqlx::Error>;

    async fn insert_cache_entry(
        &self,
        artifact_path: &str,
        file_path: Option<&str>,
        size_bytes: Option<i64>,
        content_type: &str,
        source_repository: Option<&str>,
        is_negative_cache: bool,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<(), sqlx::Error>;
}

#[async_trait]
pub trait CacheDBCleanup {
    async fn cleanup_expired_cache<F: FileDeleter + Send + Sync>(
        &self,
        cache_ttl: Duration,
        max_size_bytes: u64,
        number_of_files_to_delete: u32,
        delete_file_callback: &F,
    ) -> Result<(), Box<dyn Error>>;
    async fn remove_expired_cache_entries_by_ttl<F: FileDeleter + Send + Sync>(
        &self,
        delete_file_callback: &F,
        ttl_cutoff: DateTime<Utc>,
    ) -> Result<(), Box<dyn Error>>;
    async fn remove_expired_negative_entries(&self) -> Result<(), Box<dyn Error>>;
    async fn ensure_cache_wont_exceed_space<F: FileDeleter + Send + Sync>(
        &self,
        max_size_bytes: u64,
        number_of_files_to_delete: u32,
        delete_file_callback: &F,
    ) -> Result<(), Box<dyn Error>>;
    async fn remove_cache_entry<F: FileDeleter + Send + Sync>(
        &self,
        delete_file_callback: &F,
        candidate: &CacheEntry,
    ) -> Result<(), Box<dyn Error>>;
    async fn get_deletion_candidates(
        &self,
        number_of_files_to_delete: u32,
    ) -> Result<Vec<CacheEntry>, sqlx::Error>;
}

#[async_trait]
pub trait CacheDBStats {
    async fn get_cache_stats(&self) -> Result<CacheStats, sqlx::Error>;
}

impl DownloadTracker {
    pub fn new(pool: PgPool) -> Self {
        let (tx, mut rx) = mpsc::channel::<String>(10000);
        let pool = pool.clone();
        let s = Self { sender: tx };

        let s_clone = s.clone();
        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(1));
            let mut counts: HashMap<String, i32> = HashMap::new();
            loop {
                tokio::select! {
                    Some(artifact_id) = rx.recv() => {
                            *counts.entry(artifact_id).or_insert(0) += 1;
                    }

                    _ = ticker.tick() => {
                        if counts.is_empty() {
                            continue;
                        }
                        debug!("Flushing {} fetches to DB", counts.len());
                        // Batch updates to the DB
                        if let Err(e) = s_clone.update_counts(&pool, &counts).await {
                            eprintln!("Failed to update DB: {}", e);
                        }
                        counts.clear();
                    }
                }
            }
        });

        s
    }

    /// Called from anywhere to queue a download
    pub async fn add_download(&self, artifact_id: &str) {
        if let Err(e) = self.sender.send(artifact_id.to_string()).await {
            eprintln!("Failed to send download event: {}", e);
        }
    }
}

#[async_trait]
impl CacheDBUpdateCounter for DownloadTracker {
    async fn update_counts(
        &self,
        pool: &PgPool,
        counts: &HashMap<String, i32>,
    ) -> Result<(), sqlx::Error> {
        let tx = pool.begin().await?;
        for (artifact_id, count) in counts {
            debug!("Add {} downloads to {}", count, artifact_id);
            sqlx::query("UPDATE cache_entries SET downloads = downloads + $1, last_accessed = NOW() WHERE artifact_path = $2")
                .bind(count)
                .bind(artifact_id)
                .execute(pool)
                .await?;
        }

        tx.commit().await?;
        Ok(())
    }
}

impl CacheDatabase {
    //
    pub async fn new(database_url: &str) -> Result<Self, Box<dyn Error>> {
        info!("Connecting to database: {}", database_url);
        let db_pool = PgPool::connect(database_url).await?;
        let database = CacheDatabase {
            db_pool: db_pool.clone(),
            tracker: DownloadTracker::new(db_pool),
        };
        database.init_database().await?;
        info!("Database ready to use");
        Ok(database)
    }

    async fn init_database(&self) -> Result<(), sqlx::Error> {
        debug!("Creating table and indices, if not existing");
        sqlx::query(
            r#"
        CREATE TABLE IF NOT EXISTS cache_entries (
            id BIGSERIAL PRIMARY KEY,
            artifact_path TEXT NOT NULL UNIQUE,
            file_path TEXT,
            size_bytes BIGINT,
            downloads BIGINT,
            content_type TEXT NOT NULL DEFAULT 'application/octet-stream',
            source_repository TEXT,
            created_at TIMESTAMP WITH TIME ZONE DEFAULT NOW(),
            last_accessed TIMESTAMP WITH TIME ZONE DEFAULT NOW(),
            last_modified TIMESTAMP WITH TIME ZONE DEFAULT NOW(),
            is_negative_cache BOOLEAN DEFAULT FALSE,
            expires_at TIMESTAMP WITH TIME ZONE
        );
        "#,
        )
        .execute(&self.db_pool)
        .await?;

        sqlx::query(
            r#"
        CREATE INDEX IF NOT EXISTS idx_cache_entries_artifact_path ON cache_entries(artifact_path);
        "#,
        )
        .execute(&self.db_pool)
        .await?;

        sqlx::query(
            r#"
        CREATE INDEX IF NOT EXISTS idx_cache_entries_expires_at ON cache_entries(expires_at);
        "#,
        )
        .execute(&self.db_pool)
        .await?;

        sqlx::query(
            r#"
        CREATE INDEX IF NOT EXISTS idx_cache_entries_is_negative ON cache_entries(is_negative_cache);
        "#,
        )
            .execute(&self.db_pool)
            .await?;

        sqlx::query(
            r#"
        CREATE INDEX IF NOT EXISTS idx_cache_entries_last_accessed ON cache_entries(last_accessed);
        "#,
        )
        .execute(&self.db_pool)
        .await?;

        info!("Database schema initialized");
        Ok(())
    }
}

#[async_trait]
impl CacheDBAccess for CacheDatabase {
    async fn get_cache_entry(
        &self,
        artifact_path: &str,
    ) -> Result<Option<CacheEntry>, sqlx::Error> {
        sqlx::query_as::<_, CacheEntry>("SELECT * FROM cache_entries WHERE artifact_path = $1")
            .bind(artifact_path)
            .fetch_optional(&self.db_pool)
            .await
    }
    async fn update_last_accessed(&self, artifact_path: &str) -> Result<(), sqlx::Error> {
        self.tracker.add_download(artifact_path).await;
        Ok(())
    }

    async fn insert_cache_entry(
        &self,
        artifact_path: &str,
        file_path: Option<&str>,
        size_bytes: Option<i64>,
        content_type: &str,
        source_repository: Option<&str>,
        is_negative_cache: bool,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
        INSERT INTO cache_entries
        (artifact_path, file_path, size_bytes, content_type, source_repository, is_negative_cache, expires_at, downloads)
        VALUES ($1, $2, $3, $4, $5, $6, $7, 1)
        ON CONFLICT (artifact_path)
        DO UPDATE SET
            file_path = EXCLUDED.file_path,
            size_bytes = EXCLUDED.size_bytes,
            content_type = EXCLUDED.content_type,
            source_repository = EXCLUDED.source_repository,
            is_negative_cache = EXCLUDED.is_negative_cache,
            expires_at = EXCLUDED.expires_at,
            downloads = EXCLUDED.downloads+1,
            last_modified = NOW()
        "#,
        )
            .bind(&artifact_path)
            .bind(file_path.as_deref())
            .bind(size_bytes)
            .bind(&content_type)
            .bind(source_repository.as_deref())
            .bind(is_negative_cache)
            .bind(expires_at)
            .execute(&self.db_pool)
            .await?;

        Ok(())
    }
}

#[async_trait]
impl CacheDBCleanup for CacheDatabase {
    async fn cleanup_expired_cache<F: FileDeleter + Send + Sync>(
        &self,
        cache_ttl: Duration,
        max_size_bytes: u64,
        number_of_files_to_delete: u32,
        delete_file_callback: &F,
    ) -> Result<(), Box<dyn Error>> {
        // only delete entries from DB, no local file exists for them
        self.remove_expired_negative_entries().await?;

        // Clean up old cache entries based on TTL
        let ttl_cutoff = Utc::now() - cache_ttl;
        self.remove_expired_cache_entries_by_ttl(delete_file_callback, ttl_cutoff)
            .await?;

        if number_of_files_to_delete > 0 {
            self.ensure_cache_wont_exceed_space(
                max_size_bytes,
                number_of_files_to_delete,
                delete_file_callback,
            )
            .await?;
        }
        Ok(())
    }

    async fn remove_expired_cache_entries_by_ttl<F: FileDeleter + Send + Sync>(
        &self,
        delete_file_callback: &F,
        ttl_cutoff: DateTime<Utc>,
    ) -> Result<(), Box<dyn Error>> {
        let expired_entries = sqlx::query(
            "SELECT file_path FROM cache_entries WHERE last_accessed < $1 AND NOT is_negative_cache"
        )
            .bind(ttl_cutoff)
            .fetch_all(&self.db_pool)
            .await?;

        for row in expired_entries {
            if let Some(file_path) = row.get::<Option<String>, _>("file_path") {
                if let Err(e) = delete_file_callback
                    .delete_file(&PathBuf::from(&file_path))
                    .await
                {
                    warn!("Failed to remove cached file {:?}: {}", file_path, e);
                }
            }
        }

        // Also: clean up old cache entries based on TTL
        let cleanup_result = sqlx::query(
            "DELETE FROM cache_entries WHERE last_accessed < $1 AND NOT is_negative_cache",
        )
        .bind(ttl_cutoff)
        .execute(&self.db_pool)
        .await?;

        if cleanup_result.rows_affected() > 0 {
            info!(
                "Cleaned up {} old cache entries",
                cleanup_result.rows_affected()
            );
        }

        Ok(())
    }

    async fn remove_expired_negative_entries(&self) -> Result<(), Box<dyn Error>> {
        let result = sqlx::query(
            r#"
                    DELETE FROM cache_entries
                    WHERE expires_at IS NOT NULL AND expires_at < NOW()
                    "#,
        )
        .execute(&self.db_pool)
        .await?;

        if result.rows_affected() > 0 {
            info!(
                "Cleaned up {} expired cache entries",
                result.rows_affected()
            );
        }
        Ok(())
    }

    async fn ensure_cache_wont_exceed_space<F: FileDeleter + Send + Sync>(
        &self,
        max_size_bytes: u64,
        number_of_files_to_delete: u32,
        delete_file_callback: &F,
    ) -> Result<(), Box<dyn Error>> {
        while let Ok(cache) = self.get_cache_stats().await
            && cache.total_size_bytes as u64 > max_size_bytes
        {
            info!(
                "Cache exceeds target size: {} expected {}",
                cache.total_size_bytes, max_size_bytes
            );
            let candidates = self
                .get_deletion_candidates(number_of_files_to_delete)
                .await?;
            for candidate in candidates.iter().filter(|c| c.file_path.is_some()) {
                self.remove_cache_entry(delete_file_callback, &candidate)
                    .await?;
            }
        }
        Ok(())
    }

    async fn remove_cache_entry<F: FileDeleter + Send + Sync>(
        &self,
        delete_file_callback: &F,
        candidate: &CacheEntry,
    ) -> Result<(), Box<dyn Error>> {
        debug!(
            "Deleting old data to reduce size: {} with {} bytes last_accessed {} {} times",
            candidate.artifact_path,
            candidate.size_bytes.unwrap_or(0),
            candidate.last_accessed,
            candidate.downloads
        );
        if let Some(path) = candidate.file_path.clone() {
            if let Err(e) = delete_file_callback
                .delete_file(&PathBuf::from(&path))
                .await
            {
                warn!("Failed to remove cached file {:?}: {}", path, e);
            }
            let cleanup_result = sqlx::query("DELETE FROM cache_entries WHERE id= $1 ")
                .bind(candidate.id)
                .execute(&self.db_pool)
                .await?;
            if cleanup_result.rows_affected() != 1 {
                info!("unable to remove row? {}", candidate.id)
            }
        }
        Ok(())
    }

    async fn get_deletion_candidates(
        &self,
        number_of_files_to_delete: u32,
    ) -> Result<Vec<CacheEntry>, sqlx::Error> {
        sqlx::query_as::<_, CacheEntry>(
            "SELECT * FROM cache_entries where is_negative_cache = false order by (now() - last_accessed) / downloads desc limit $1",
        )
            .bind(number_of_files_to_delete as i32)
            .fetch_all(&self.db_pool)
            .await
    }
}

#[async_trait]
impl CacheDBStats for CacheDatabase {
    async fn get_cache_stats(&self) -> Result<CacheStats, sqlx::Error> {
        let stats = sqlx::query(
            r#"
        SELECT
            cast(COUNT(*) as BIGINT) as total_entries,
            cast(COUNT(*) FILTER (WHERE is_negative_cache) as BIGINT) as negative_entries,
            cast(COUNT(*) FILTER (WHERE NOT is_negative_cache) as BIGINT) as positive_entries,
            cast(COALESCE(SUM(size_bytes), 0) as BIGINT) as total_size_bytes
        FROM cache_entries
        "#,
        )
        .fetch_one(&self.db_pool)
        .await?;

        Ok(CacheStats {
            total_entries: stats.get("total_entries"),
            negative_entries: stats.get("negative_entries"),
            positive_entries: stats.get("positive_entries"),
            total_size_bytes: stats.get("total_size_bytes"),
        })
    }
}

#[cfg(test)]
#[cfg(feature = "test_containers")]
mod tests {
    use super::*;
    use chrono::Local;
    use std::sync::Arc;

    use testcontainers::runners::AsyncRunner;
    use testcontainers::{ContainerAsync, ImageExt};
    use testcontainers_modules::postgres::Postgres;
    use tokio::sync::Mutex;
    use tokio::time::sleep;

    struct DummyCallback {
        items: Arc<Mutex<Vec<String>>>,
    }

    impl Default for DummyCallback {
        fn default() -> Self {
            DummyCallback {
                items: Arc::new(Mutex::new(vec![])),
            }
        }
    }

    #[async_trait]
    impl FileDeleter for DummyCallback {
        async fn delete_file(&self, path: &PathBuf) -> Result<(), Box<dyn Error>> {
            let path_str = path.to_string_lossy().to_string();
            self.items.lock().await.push(path_str);
            Ok(())
        }
    }

    async fn create_container() -> ContainerAsync<Postgres> {
        Postgres::default().with_tag("17").start().await.unwrap()
    }

    async fn create_test_db(container: &ContainerAsync<Postgres>) -> CacheDatabase {
        let host_port = container.get_host_port_ipv4(5432).await.unwrap();
        info!("host_port is {}", host_port);
        let connection_string =
            &format!("postgres://postgres:postgres@127.0.0.1:{host_port}/postgres");
        CacheDatabase::new(connection_string).await.unwrap()
    }

    #[tokio::test]
    async fn prepare_test() {
        tracing_subscriber::fmt::init();
    }

    async fn create_test_artifacts(db: &CacheDatabase) {
        let test_entries = vec![
            // Regular cache entries with various access patterns
            (
                "artifact1",
                1,
                100,
                Some(10_000),
                false,
                None,
                Some("artifact1.txt"),
            ),
            (
                "artifact2",
                2,
                10,
                Some(20_000),
                false,
                None,
                Some("artifact2.txt"),
            ),
            (
                "artifact3",
                10,
                100,
                Some(50_000),
                false,
                None,
                Some("artifact3.txt"),
            ),
            (
                "artifact4",
                1000,
                10,
                Some(90_000),
                false,
                None,
                Some("artifact4.txt"),
            ),
            // Negative cache entry
            (
                "artifact5",
                1000,
                100,
                Some(2_000),
                true,
                None,
                Some("artifact5.txt"),
            ),
            // Expired negative cache entry
            (
                "artifact6",
                1000,
                10,
                None,
                true,
                Some(Local::now() - Duration::new(3600, 0)),
                Some("expired_neg.txt"),
            ),
            // Entry with expired TTL
            (
                "artifact7",
                900,
                900,
                Some(1_000),
                false,
                None,
                Some("expired_ttl.txt"),
            ),
        ];

        for (
            path,
            download_count,
            last_accessed_secs,
            size_in_bytes,
            is_negative,
            expires_at,
            file_path,
        ) in test_entries
        {
            sqlx::query(
                r#"
                    INSERT INTO cache_entries (artifact_path, downloads, last_accessed, size_bytes, is_negative_cache, expires_at, file_path)
                    VALUES ($1, $2, $3, $4, $5, $6, $7)
                    "#,
                )
                .bind(path)
                .bind(download_count)
                .bind(Local::now() - Duration::new(last_accessed_secs, 0))
                .bind(size_in_bytes)
                .bind(is_negative)
                .bind(expires_at)
                .bind(file_path)
                .execute(&db.db_pool)
                .await
                .expect("Failed to insert test data");
        }
    }

    #[tokio::test]
    async fn test_get_stats() {
        let container = create_container().await;
        let db = create_test_db(&container).await;

        create_test_artifacts(&db).await;

        let stats = db.get_cache_stats().await.unwrap();

        assert_eq!(stats.negative_entries, 2);
        assert_eq!(stats.positive_entries, 5);
        assert_eq!(stats.total_entries, 7);
        assert_eq!(stats.total_size_bytes, 173_000);
    }

    #[tokio::test]
    async fn test_get_least_significant_artifact_for_deletion() {
        let container = create_container().await;
        let db = create_test_db(&container).await;

        create_test_artifacts(&db).await;

        let entries = db.get_deletion_candidates(3).await.unwrap();

        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].artifact_path, "artifact1");
        assert_eq!(entries[1].artifact_path, "artifact3");
        assert_eq!(entries[2].artifact_path, "artifact2");
    }

    #[tokio::test]
    async fn test_increment_download_counter() {
        let container = create_container().await;
        // Initialize tracing
        let db = create_test_db(&container).await;
        db.insert_cache_entry(
            "test_download",
            Some("test_download"),
            Some(1234),
            "application/octet-stream",
            Some("maven"),
            false,
            None,
        )
        .await
        .unwrap();

        let previous = db.get_cache_entry("test_download").await.unwrap().unwrap();
        assert_eq!(previous.downloads, 1);

        db.update_last_accessed("test_download").await.unwrap();
        sleep(Duration::from_secs(2)).await;
        let current = db.get_cache_entry("test_download").await.unwrap().unwrap();
        assert_eq!(current.downloads, 2);
        assert_eq!(current.last_accessed > previous.last_accessed, true);
    }

    #[tokio::test]
    async fn test_remove_cache_entry() {
        let container = create_container().await;
        let db = create_test_db(&container).await;

        // Insert test artifact with file -ath
        let test_artifact = "test_remove_entry";
        let test_file_path = "test_file.jar";

        db.insert_cache_entry(
            test_artifact,
            Some(test_file_path),
            Some(5000),
            "application/java-archive",
            Some("maven-central"),
            false,
            None,
        )
        .await
        .unwrap();

        // Get the entry to pass to remove_cache_entry
        let entry = db.get_cache_entry(test_artifact).await.unwrap().unwrap();

        let delete_callback = DummyCallback::default();

        // Remove the entry
        db.remove_cache_entry(&delete_callback, &entry)
            .await
            .unwrap();

        // Verify file deletion was attempted
        let files = delete_callback.items.lock().await;

        assert_eq!(files.len(), 1);
        assert!(files[0].contains(test_file_path));

        // Verify entry was removed from the database
        let result = db.get_cache_entry(test_artifact).await.unwrap();
        assert!(result.is_none(), "Cache entry should have been removed");
    }

    #[tokio::test]
    async fn test_remove_expired_negative_entries() {
        let container = create_container().await;
        let db = create_test_db(&container).await;

        create_test_artifacts(&db).await;

        // Get initial counts
        let stats_before = db.get_cache_stats().await.unwrap();

        // Run the function to remove expired negative entries
        db.remove_expired_negative_entries().await.unwrap();

        // Get updated counts
        let stats_after = db.get_cache_stats().await.unwrap();

        assert!(
            stats_after.negative_entries < stats_before.negative_entries,
            "Expected fewer negative entries after cleanup"
        );

        let expired_entry = db.get_cache_entry("artifact6").await.unwrap();
        assert!(
            expired_entry.is_none(),
            "Expired negative entry should be removed"
        );
        let valid_entry = db.get_cache_entry("artifact5").await.unwrap();
        assert!(
            valid_entry.is_some(),
            "Non-expired negative entry should remain"
        );
        assert!(
            valid_entry.unwrap().is_negative_cache,
            "Entry should be a negative cache entry"
        );
    }

    #[tokio::test]
    async fn test_remove_expired_cache_entries_by_ttl() {
        let container = create_container().await;
        let db = create_test_db(&container).await;

        create_test_artifacts(&db).await;

        let delete_callback = DummyCallback::default();

        // Set cutoff to remove entries older than 400 seconds
        // This should remove artifact9 (500 seconds old)
        let ttl_cutoff = Utc::now() - Duration::new(400, 0);

        // Get initial state
        let stats_before = db.get_cache_stats().await.unwrap();

        // Run the TTL cleanup
        db.remove_expired_cache_entries_by_ttl(&delete_callback, ttl_cutoff)
            .await
            .unwrap();

        // Check stats after cleanup
        let stats_after = db.get_cache_stats().await.unwrap();

        let deleted_files = delete_callback.items.lock().await;
        // Verify entry count decreased
        assert!(
            stats_after.total_entries < stats_before.total_entries,
            "Expected fewer entries after TTL cleanup"
        );

        // Verify the specific entry with expired TTL was removed
        let expired_entry = db.get_cache_entry("artifact9").await.unwrap();
        assert!(
            expired_entry.is_none(),
            "Entry with access time older than TTL should be removed"
        );

        // Verify file deletion was called
        assert!(
            !deleted_files.is_empty(),
            "Expected at least one file deletion"
        );
        assert!(
            deleted_files
                .iter()
                .any(|path| path.contains("expired_ttl.txt")),
            "Expected expired_ttl.txt to be deleted"
        );

        // Verify newer entries remain
        let recent_entry = db.get_cache_entry("artifact4").await.unwrap();
        assert!(recent_entry.is_some(), "Recent entry should not be removed");
    }

    #[tokio::test]
    async fn test_ensure_cache_wont_exceed_space() {
        let container = create_container().await;
        let db = create_test_db(&container).await;

        create_test_artifacts(&db).await;

        // Track deleted files
        let delete_callback = DummyCallback::default();

        // Get initial state
        let stats_before = db.get_cache_stats().await.unwrap();

        // Set a max cache size that's smaller than current size to force cleanup
        let max_size_bytes = 100_000; // 100KB

        // Run the space cleanup, allowing up to 3 files to be deleted
        db.ensure_cache_wont_exceed_space(max_size_bytes, 3, &delete_callback)
            .await
            .unwrap();

        // Check stats after cleanup
        let stats_after = db.get_cache_stats().await.unwrap();

        let deleted_files = delete_callback.items.lock().await;
        assert!(
            stats_after.total_size_bytes < stats_before.total_size_bytes,
            "Expected smaller cache size after cleanup"
        );

        assert!(
            stats_after.total_size_bytes as u64 <= max_size_bytes,
            "Cache size should not exceed the maximum after cleanup"
        );

        assert!(
            !deleted_files.is_empty(),
            "Expected at least one file deletion"
        );

        // Check which entries remain vs which were deleted
        // The entries with the highest value (downloads / age) should remain
        let artifact4 = db.get_cache_entry("artifact4").await.unwrap();
        assert!(
            artifact4.is_some(),
            "High-value entry artifact4 should remain"
        );
    }

    #[tokio::test]
    async fn test_cleanup_expired_cache() {
        let container = create_container().await;
        let db = create_test_db(&container).await;

        create_test_artifacts(&db).await;

        let delete_callback = DummyCallback::default();

        // Get initial state
        let stats_before = db.get_cache_stats().await.unwrap();

        // Configure cleanup parameters
        let cache_ttl = Duration::new(300, 0); // 5 minutes
        let max_size_bytes = 100_000; // 100KB - smaller than our test data
        let number_of_files_to_delete = 3;

        // Run the complete cleanup process
        db.cleanup_expired_cache(
            cache_ttl,
            max_size_bytes,
            number_of_files_to_delete,
            &delete_callback,
        )
        .await
        .unwrap();

        // Check stats after cleanup
        let stats_after = db.get_cache_stats().await.unwrap();

        let deleted_files = delete_callback.items.lock().await;
        assert!(
            stats_after.total_entries < stats_before.total_entries,
            "Expected fewer entries after cleanup"
        );

        assert!(
            stats_after.total_size_bytes as u64 <= max_size_bytes,
            "Cache size should not exceed the maximum after cleanup"
        );

        assert!(
            !deleted_files.is_empty(),
            "Expected at least one file deletion"
        );

        let expired_neg_entry = db.get_cache_entry("artifact6").await.unwrap();
        assert!(
            expired_neg_entry.is_none(),
            "Expired negative entry should be removed"
        );

        let expired_ttl_entry = db.get_cache_entry("artifact9").await.unwrap();
        assert!(
            expired_ttl_entry.is_none(),
            "Entry with access time older than TTL should be removed"
        );

        let high_value_entry = db.get_cache_entry("artifact4").await.unwrap();
        assert!(high_value_entry.is_some(), "High-value entry should remain");
    }
}
