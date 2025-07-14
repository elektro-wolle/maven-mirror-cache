use crate::cache_database::{CacheDBAccess, CacheDatabase};
use crate::config::Repository;
use crate::remote_fetcher::RemoteFetcher;
use crate::utils::{guess_content_type, sanitize_path};
use crate::AppState;
use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum_server_timing::ServerTimingExtension;
use futures::{StreamExt, TryStreamExt};
use http_body_util::StreamBody;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;
use tokio::fs;
use tokio::fs::File;
use tokio_util::io::ReaderStream;
use tracing::{debug, error, info, trace, warn};

// Result of a repository fetch attempt
#[derive(Debug)]
pub struct FetchResult {
    pub downloaded_file: NamedTempFile,
    pub content_type: String,
    pub repository_name: String,
}

#[derive(Clone)]
pub struct Cache {
    pub database: CacheDatabase,
}

impl Cache {
    /// Downloads an artifact from enabled repositories and caches it.
    ///
    /// This asynchronous function attempts to download an artifact specified by `artifact_path`
    /// from one of the enabled repositories. It uses semaphore to limit the number of concurrent
    /// downloads.
    /// If the artifact is successfully downloaded, it is cached both on disk and its metadata
    /// is stored in the database.
    /// If the artifact is not found in any repository, a negative cache entry is created in the database.
    pub async fn download_and_cache(
        &self,
        state: &AppState,
        artifact_path: &str,
        timing: &ServerTimingExtension,
    ) -> Result<Response, Box<dyn std::error::Error>> {
        // Acquire semaphore to limit concurrent downloads
        let _permit = state.download_semaphore.acquire().await?;

        let result = self
            .try_to_fetch_from_remote(&state, artifact_path, timing)
            .await?;

        match result {
            Some(fetch_result) => {
                let (return_headers, file) =
                    Self::save_artifact_in_cache_dir(state, artifact_path, timing, &fetch_result)
                        .await?;

                match File::open(&file).await {
                    Ok(file) => {
                        // file is in the local cache -> serve directly
                        Self::stream_file_with_headers(return_headers, file)
                    }
                    Err(e) => {
                        Err(format!("Artifact {} not found in cache?: {}", artifact_path, e).into())
                    }
                }
            }
            None => {
                Self::save_not_found_artifact_in_cache_db(state, artifact_path, timing).await;
                Err(format!("Artifact {} not found in any repository", artifact_path).into())
            }
        }
    }

    fn stream_file_with_headers(
        return_headers: HeaderMap,
        file: File,
    ) -> Result<Response, Box<dyn std::error::Error>> {
        let stream = ReaderStream::new(file);
        let body = StreamBody::new(stream).map_err(|e| e.to_string()).boxed();
        let mut response = Response::builder();
        let headers = response.headers_mut();
        if let Some(headers) = headers {
            return_headers.iter().for_each(|(k, v)| {
                headers.insert(k, v.clone());
            })
        }
        Ok(response
            .status(StatusCode::OK)
            .body(Body::from_stream(body))?)
    }

    async fn save_not_found_artifact_in_cache_db(
        state: &AppState,
        artifact_path: &str,
        timing: &ServerTimingExtension,
    ) {
        // No repository had the artifact, cache the negative result
        let expires_at = chrono::Utc::now() + state.config.negative_ttl;

        let timer_start = Instant::now();
        if let Err(e) = state
            .database
            .insert_cache_entry(
                artifact_path,
                None,
                None,
                "text/plain",
                None,
                true,
                Some(expires_at),
            )
            .await
        {
            error!("Failed to store negative cache entry: {}", e);
        }
        timing.lock().unwrap().record_timing(
            "store_negative".to_string(),
            timer_start.elapsed(),
            None,
        );

        debug!(
            "Cached negative result for {} (expires at {})",
            artifact_path, expires_at
        );
    }

    async fn save_artifact_in_cache_dir(
        state: &AppState,
        artifact_path: &str,
        timing: &ServerTimingExtension,
        fetch_result: &FetchResult,
    ) -> Result<(HeaderMap, PathBuf), Box<dyn std::error::Error>> {
        let timer_start = Instant::now();
        // save in the local cache directory
        let relative_path = PathBuf::from(artifact_path);
        let cache_path = state.config.cache_dir.join(&relative_path);
        let parent_dir = cache_path.parent();
        if let Some(dir) = parent_dir {
            fs::create_dir_all(dir).await?;
        } else {
            return Err("Unable to determine parent directory".into());
        }
        fs::rename(&fetch_result.downloaded_file, &cache_path).await?;
        let metadata = fs::metadata(&cache_path).await?;
        let size_bytes = metadata.len();

        timing.lock().unwrap().record_timing(
            "store_artifact".to_string(),
            timer_start.elapsed(),
            None,
        );

        // record the metadata in the database
        if let Err(e) = state
            .database
            .insert_cache_entry(
                artifact_path,
                Some(&relative_path.to_string_lossy()),
                Some(size_bytes as i64),
                &fetch_result.content_type,
                Some(&fetch_result.repository_name),
                false,
                None,
            )
            .await
        {
            error!("Failed to store cache entry in database: {}", e);
        }
        timing
            .lock()
            .unwrap()
            .record_timing("save_to_db".to_string(), timer_start.elapsed(), None);

        info!(
            "Cached artifact: {} from {} ({} bytes)",
            artifact_path, fetch_result.repository_name, size_bytes
        );

        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            fetch_result.content_type.parse().unwrap(),
        );
        headers.insert("X-Cache", "MISS".parse().unwrap());
        headers.insert(
            "X-Source-Repository",
            fetch_result.repository_name.parse().unwrap(),
        );
        Ok((headers, cache_path))
    }

    async fn try_to_fetch_from_remote(
        &self,
        state: &AppState,
        artifact_path: &str,
        timing: &ServerTimingExtension,
    ) -> Result<Option<FetchResult>, Box<dyn std::error::Error>> {
        info!(
            "Trying to download {} from {} repositories",
            artifact_path,
            state.config.repositories.len()
        );
        let timer_start = Instant::now();
        let remote_file =
            Self::generate_download_futures(state, artifact_path, &state.config.repositories).await;
        timing.lock().unwrap().record_timing(
            "prepare_download".to_string(),
            timer_start.elapsed(),
            None,
        );

        let timer_start = Instant::now();
        // Cache the successful result
        timing.lock().unwrap().record_timing(
            "fetch_artifact".to_string(),
            timer_start.elapsed(),
            None,
        );
        Ok(remote_file)
    }

    /// Create a vec of futures, each entry representing the download attempt to one
    /// of the configured remote repositories.
    ///
    /// The priority between the repositories is handled by an artificial sleep.
    async fn generate_download_futures(
        state: &AppState,
        artifact_path: &str,
        enabled_repos: &Vec<Repository>,
    ) -> Option<FetchResult> {
        for repo in enabled_repos {
            let client = state.http_client.clone();
            let artifact_path = artifact_path.to_string();

            let url = format!("{}/{}", repo.url, artifact_path);
            let repo_name = repo.name.clone();
            let timeout = repo.timeout.unwrap_or(Duration::new(30, 0));
            let headers = repo.headers.clone();
            let temp_file = NamedTempFile::new_in(&state.config.cache_dir);
            // 10 ms per prio

            let mut request = client.get(&url).timeout(timeout);

            if let Some(headers_map) = headers {
                for (key, value) in headers_map {
                    request = request.header(&key, &value);
                }
            }

            info!("Fetching from {}: {}", repo_name, url);
            match temp_file {
                Ok(temp_file) => {
                    match Self::fetch_from_remote(temp_file, artifact_path, repo_name, request)
                        .await
                    {
                        Some(fetch_result) => return Some(fetch_result),
                        None => continue,
                    }
                }
                Err(e) => {
                    error!("Failed to create temporary file: {}", e);
                }
            }
        }
        None
    }

    /// Fire the request to the remote server, awaiting the response.
    /// Returns the loaded data as bytes and some metadata (content-type and repo-name)

    /// Try to load
    ///
    pub async fn try_serve_from_cache(
        &self,
        state: &AppState,
        artifact_path: &str,
        timing: &ServerTimingExtension,
    ) -> Option<Response> {
        let timer_start = Instant::now();
        let path = sanitize_path(PathBuf::from(artifact_path));
        trace!("Sanitized artifact path: {:?} to {:?}", artifact_path, path);
        let full_path = state.config.cache_dir.join(path);
        match File::open(&full_path).await {
            Ok(file) => {
                let file_size = file.metadata().await;
                if file_size.is_err() {
                    return None;
                }
                info!("Serving cached artifact: {}", artifact_path);
                let file_size = file_size.unwrap().len();

                // file is in the local cache -> serve directly
                timing.lock().unwrap().record_timing(
                    "open_file".to_string(),
                    timer_start.elapsed(),
                    None,
                );

                let _ = state
                    .database
                    .update_last_accessed(artifact_path)
                    .await
                    .map_err(|e| {
                        warn!("failed to update last accessed of {}: {}", artifact_path, e)
                    });

                let mut headers = HeaderMap::new();
                headers.append(header::CONTENT_LENGTH, file_size.into());
                headers.append(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static(guess_content_type(&artifact_path)),
                );
                headers.append("X-Cache", HeaderValue::from_static("HIT"));

                let response = Self::stream_file_with_headers(headers, file);

                return Some(response.unwrap_or_else(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Unable to fetch cache file: {}", e),
                    )
                        .into_response()
                }));
            }
            Err(_) => {
                debug!("artifact not cached as file: {:?}", artifact_path);
            }
        };

        // Not found in filesystem, maybe a negative entry is present?
        match state.database.get_cache_entry(artifact_path).await {
            Ok(None) => return None,
            Ok(Some(entry)) => {
                timing.lock().unwrap().record_timing(
                    "got_cache_from_db".to_string(),
                    timer_start.elapsed(),
                    None,
                );

                // Check if entry is expired
                if let Some(expires_at) = entry.expires_at {
                    if expires_at < chrono::Utc::now() {
                        debug!("got expired artifact from db: {}", artifact_path);
                        return None;
                    }
                }

                if entry.is_negative_cache {
                    // Return 404 for negative cache entries
                    info!("Serving negative cache entry for {}", artifact_path);
                    let _ = state
                        .database
                        .update_last_accessed(artifact_path)
                        .await
                        .map_err(|e| {
                            warn!("failed to update last accessed of {}: {}", artifact_path, e)
                        });

                    return Some(
                        (
                            StatusCode::NOT_FOUND,
                            "Artifact not found in any repository",
                        )
                            .into_response(),
                    );
                }
            }
            Err(e) => {
                error!("Database error while checking cache: {}", e);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {}
