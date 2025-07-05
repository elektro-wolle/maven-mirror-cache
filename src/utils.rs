use crate::cache::FetchResult;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use std::path::{Component, PathBuf};
use tokio::task::JoinHandle;
use tracing::info;

/// Asynchronously finds the first successful result from a list of futures.
///
/// This function takes a vector of `JoinHandle<Option<FetchResult>>` and returns the first successful result
/// encountered. It uses `FuturesUnordered` to manage the futures concurrently. If a future returns a successful
/// result, all remaining futures are aborted.
///
/// # Arguments
///
/// * `futures` - A vector of `JoinHandle<Option<FetchResult>>` representing asynchronous tasks.
///
/// # Returns
///
/// * `Result<Option<FetchResult>, Box<dyn std::error::Error>>` - The first successful result wrapped in `Ok(Some(val))`.
///   If all futures complete without a successful result, it returns `Ok(None)`. If an error occurs during execution,
///   it logs the error and continues processing the remaining futures.
///
/// # Examples
///
/// ```rust
/// use tokio::spawn;
/// use tokio::task::JoinHandle;
/// use futures::future::join_all;
///
/// async fn fetch_data() -> Option<FetchResult> {
///     // Simulate fetching data
///     Some(FetchResult { /* fields */ })
/// }
///
/// async fn example_usage() {
///     let futures: Vec<JoinHandle<Option<FetchResult>>> = vec![
///         spawn(fetch_data()),
///         spawn(fetch_data()),
///     ];
///     let result = find_first(futures).await;
///     println!("Result: {:?}", result);
/// }
/// ```
pub async fn find_first(
    futures: Vec<JoinHandle<Option<FetchResult>>>,
) -> Result<Option<FetchResult>, Box<dyn std::error::Error>> {
    let mut futures_unordered = FuturesUnordered::new();
    for fut in futures {
        futures_unordered.push(fut);
    }

    while let Some(res) = futures_unordered.next().await {
        match res {
            Ok(Some(val)) => {
                futures_unordered.into_iter().for_each(|o| {
                    if !o.is_finished() {
                        o.abort();
                    }
                });
                return Ok(Some(val));
            }
            Ok(None) => {
                continue;
            }
            Err(err) => {
                info!("error while selecting: {}", err);
            }
        }
    }
    Ok(None)
}

pub fn sanitize_path(path: PathBuf) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                // Handle ".." by popping the last component if possible
                if !result.as_os_str().is_empty() {
                    result.pop();
                }
            }
            Component::Normal(_) => {
                // Push normal components onto the result path
                result.push(component);
            }
            _ => {
                // Ignore other components like root or prefix
            }
        }
    }
    result
}

pub fn guess_content_type(path: &str) -> &'static str {
    match path.split('.').last() {
        Some("jar") => "application/java-archive",
        Some("war") => "application/java-archive",
        Some("ear") => "application/java-archive",
        Some("pom") => "application/xml",
        Some("xml") => "application/xml",
        Some("sha1") => "text/plain",
        Some("sha256") => "text/plain",
        Some("sha512") => "text/plain",
        Some("md5") => "text/plain",
        Some("asc") => "text/plain",
        Some("txt") => "text/plain",
        Some("properties") => "text/plain",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::task::JoinHandle;
    use tokio::time::sleep;

    #[tokio::test]
    async fn test_sanitize_path() {
        // Test normal path
        let path = PathBuf::from("normal/path/to/file.jar");
        assert_eq!(
            sanitize_path(path),
            PathBuf::from("normal/path/to/file.jar")
        );

        // Test path with parent references
        let path = PathBuf::from("some/path/../to/file.jar");
        assert_eq!(sanitize_path(path), PathBuf::from("some/to/file.jar"));

        // Test path with multiple parent references
        let path = PathBuf::from("deep/path/../../to/file.jar");
        assert_eq!(sanitize_path(path), PathBuf::from("to/file.jar"));

        // Test path attempting to escape
        let path = PathBuf::from("../../../etc/passwd");
        assert_eq!(sanitize_path(path), PathBuf::from("etc/passwd"));
    }

    #[tokio::test]
    async fn test_find_first_with_success() {
        // Create a Vec of futures where one returns Some
        let futures: Vec<JoinHandle<Option<FetchResult>>> = vec![
            tokio::spawn(async {
                sleep(Duration::from_millis(50)).await;
                None
            }),
            tokio::spawn(async {
                sleep(Duration::from_millis(10)).await;
                Some(FetchResult {
                    content: bytes::Bytes::from("test content"),
                    content_type: "text/plain".to_string(),
                    repository_name: "test-repo".to_string(),
                })
            }),
            tokio::spawn(async {
                sleep(Duration::from_millis(100)).await;
                Some(FetchResult {
                    content: bytes::Bytes::from("should not be returned"),
                    content_type: "text/plain".to_string(),
                    repository_name: "another-repo".to_string(),
                })
            }),
        ];

        let result = find_first(futures).await.unwrap();
        assert!(result.is_some());
        let fetch_result = result.unwrap();
        assert_eq!(fetch_result.content, bytes::Bytes::from("test content"));
        assert_eq!(fetch_result.repository_name, "test-repo");
    }

    #[tokio::test]
    async fn test_find_first_with_all_none() {
        // Create a Vec of futures where all return None
        let futures: Vec<JoinHandle<Option<FetchResult>>> = vec![
            tokio::spawn(async {
                sleep(Duration::from_millis(10)).await;
                None
            }),
            tokio::spawn(async {
                sleep(Duration::from_millis(20)).await;
                None
            }),
        ];

        let result = find_first(futures).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_find_first_with_errors() {
        // Create a Vec of futures where some panic
        let futures: Vec<JoinHandle<Option<FetchResult>>> = vec![
            tokio::spawn(async {
                panic!("This future panics");
                #[allow(unreachable_code)]
                None
            }),
            tokio::spawn(async {
                sleep(Duration::from_millis(10)).await;
                None
            }),
            tokio::spawn(async {
                sleep(Duration::from_millis(20)).await;
                Some(FetchResult {
                    content: bytes::Bytes::from("valid result"),
                    content_type: "text/plain".to_string(),
                    repository_name: "error-test-repo".to_string(),
                })
            }),
        ];

        let result = find_first(futures).await.unwrap();
        assert!(result.is_some());
        let fetch_result = result.unwrap();
        assert_eq!(fetch_result.content, bytes::Bytes::from("valid result"));
        assert_eq!(fetch_result.repository_name, "error-test-repo");
    }
}
