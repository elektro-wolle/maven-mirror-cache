extern crate core;

mod cache;
mod cache_database;
mod config;
mod remote_fetcher;
mod utils;

use crate::cache::Cache;
use crate::cache_database::{CacheDBCleanup, CacheDBStats, FileDeleter};
use crate::config::ProgramConfig;
use async_trait::async_trait;
use axum::{
    extract::Path, extract::State, http::StatusCode, response::IntoResponse, response::Response, routing::get,
    Extension, Router,
};
use axum_server_timing::ServerTimingExtension;
use cache_database::CacheDatabase;
use reqwest::Client;
use std::error::Error;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use std::{path::PathBuf, sync::Arc};
use tokio::{fs, signal};
use tower_http::cors::CorsLayer;
use tracing::{error, info};

// Main application state
#[derive(Clone)]
pub struct AppState {
    pub config: ProgramConfig,
    pub http_client: Client,
    pub database: CacheDatabase,
    pub cache: Cache,
    pub download_semaphore: Arc<tokio::sync::Semaphore>,
}

impl AppState {
    pub async fn new(config: ProgramConfig) -> Result<Self, Box<dyn Error>> {
        // Connect to database

        let http_client = Client::builder()
            .timeout(config.server.request_timeout)
            .user_agent("Maven-Mirror-Cache/2.0")
            .build()?;
        let database = CacheDatabase::new(&config.database_url).await?;
        let state = Self {
            download_semaphore: Arc::new(tokio::sync::Semaphore::new(
                config.max_concurrent_downloads,
            )),
            config,
            http_client,
            database: database.clone(),
            cache: Cache { database },
        };

        if state.config.repositories.is_empty() {
            return Err("No enabled repositories configured".into());
        }

        state.initialize().await?;
        Ok(state)
    }

    pub async fn initialize(&self) -> Result<(), Box<dyn Error>> {
        // Create cache directory if it doesn't exist
        fs::create_dir_all(&self.config.cache_dir).await?;

        info!(
            "Maven mirror cache initialized at {:?}",
            self.config.cache_dir
        );
        info!(
            "Configured repositories: {:?}",
            self.config
                .repositories
                .iter()
                .map(|r| &r.name)
                .collect::<Vec<_>>()
        );

        Ok(())
    }
}

//
/// Main handler for artifact requests. Tries to serve the requested artifact from the filesystem
/// and records the access. Checks the "cache-control" header, to ensure a freshly fetched artifact
/// if instructed to do so.
///
/// # Returns
///
/// * Http 200 with body of the artifact.
/// * Http 404 if the artifact is not known by any repo or that fact is already cached.
async fn handle_artifact(
    Path(artifact_path): Path<String>,
    State(state): State<AppState>,
    Extension(timing): Extension<ServerTimingExtension>,
) -> Result<Response, StatusCode> {
    let timer_start = Instant::now();
    if let Some(cached_response) = state
        .cache
        .try_serve_from_cache(&state, &artifact_path, &timing)
        .await
    {
        timing.lock().unwrap().record_timing(
            "cache_serve".to_string(),
            timer_start.elapsed(),
            None,
        );
        return Ok(cached_response);
    }

    // Download from upstream repositories
    let timer_start = Instant::now();
    match state
        .cache
        .download_and_cache(&state, &artifact_path, &timing)
        .await
    {
        Ok(response) => {
            info!(
                "Serving response for new artifact: {} = {}",
                artifact_path,
                response.status()
            );
            timing.lock().unwrap().record_timing(
                "download_and_cache".to_string(),
                timer_start.elapsed(),
                None,
            );
            Ok(response)
        }
        Err(e) => {
            info!("Artifact {} not found: {}", artifact_path, e);
            Err(StatusCode::NOT_FOUND)
        }
    }
}

// Health check endpoint
async fn health_check(State(state): State<AppState>) -> impl IntoResponse {
    match state.database.get_cache_stats().await {
        Ok(stats) => {
            let status = serde_json::json!({
            "status": "healthy",
            "cache_entries": stats.total_entries,
            "positive_entries": stats.positive_entries,
            "negative_entries": stats.negative_entries,
            "cache_size_bytes": stats.total_size_bytes,
            "cache_size_mb": stats.total_size_bytes / 1024 / 1024,
            "repositories": state.config.repositories.len(),
            });

            (StatusCode::OK, serde_json::to_string(&status).unwrap())
        }
        Err(e) => {
            error!("Failed to get cache stats: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Database error".to_string(),
            )
        }
    }
}

// Cache statistics endpoint
async fn cache_stats(State(state): State<AppState>) -> impl IntoResponse {
    match state.database.get_cache_stats().await {
        Ok(stats) => {
            let response = serde_json::json!({
            "total_entries": stats.total_entries,
            "positive_entries": stats.positive_entries,
            "negative_entries": stats.negative_entries,
            "total_size_bytes": stats.total_size_bytes,
            "total_size_mb": stats.total_size_bytes / 1024 / 1024,
            "cache_directory": state.config.cache_dir,
            "ttl": state.config.cache_ttl,
            "negative_ttl": state.config.negative_ttl.as_secs(),
            "max_cache_size": state.config.max_cache_size,
            "repositories": state.config.repositories,
            });

            (StatusCode::OK, serde_json::to_string(&response).unwrap())
        }
        Err(e) => {
            error!("Failed to get cache stats: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Database error".to_string(),
            )
        }
    }
}

// Background task for cache cleanup
async fn cache_cleanup_task(config: &ProgramConfig, database: &CacheDatabase) {
    let mut interval = tokio::time::interval(config.cleanup_interval);

    struct DeleteCallback {
        cache_dir: PathBuf,
    }

    #[async_trait]
    impl FileDeleter for DeleteCallback {
        async fn delete_file(&self, file_path: &PathBuf) -> Result<(), Box<dyn Error>> {
            let cache_dir = self.cache_dir.clone();
            let full_path = cache_dir.join(&file_path);
            let _ = fs::remove_file(&full_path)
                .await
                .map_err(|e| info!("Unable to delete file {}, {}", full_path.display(), e));
            Ok(())
        }
    }

    let delete_file = DeleteCallback {
        cache_dir: config.cache_dir.clone(),
    };
    loop {
        interval.tick().await;

        info!("Starting cache cleanup task");
        let x = database.cleanup_expired_cache(
            config.cache_ttl,
            config.max_cache_size,
            config.number_of_files_to_delete,
            &delete_file,
        );
        if let Err(e) = x.await {
            error!("Cache cleanup failed: {}", e);
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            info!("Ctrl+C received, shutting down");
        },
        _ = terminate => {
            info!("Terminate received, shutting down");
        },
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Initialize tracing
    tracing_subscriber::fmt::init();
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // Load configuration
    let config = config::load_config()?;
    fs::create_dir_all(&config.cache_dir).await?;

    // Create application state
    let state = AppState::new(config.clone()).await?;

    // Start the background cleanup task
    let cleanup_state = state.clone();
    tokio::spawn(async move {
        cache_cleanup_task(&cleanup_state.config, &cleanup_state.database).await;
    });

    // Build the router
    let app = Router::new()
        .route("/health", get(health_check))
        .route("/stats", get(cache_stats))
        .route("/{*artifact_path}", get(handle_artifact))
        .layer(CorsLayer::permissive())
        .layer(axum_server_timing::ServerTimingLayer::new("repo-cache"))
        .with_state(state);

    // Start the server
    let bind_addr = format!("{}:{}", config.server.host, config.server.port);
    match config.server.create_tls_config().await? {
        Some(tls_config) => {
            info!(
                "Maven mirror cache server starting on https://{}",
                bind_addr
            );
            let handle = axum_server::Handle::new();
            let listener: SocketAddr = bind_addr.parse()?;
            let server_future = axum_server::bind_rustls(listener, tls_config)
                .handle(handle.clone())
                .serve(app.into_make_service());
            tokio::select! {
                () = shutdown_signal() =>
                    handle.graceful_shutdown(Some(Duration::new(30, 0))),
                res = server_future =>
                    res?,
            }
        }
        None => {
            let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
            info!("Maven mirror cache server starting on http://{}", bind_addr);
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_signal())
                .await?;
        }
    }

    Ok(())
}
