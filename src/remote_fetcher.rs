use crate::cache::{Cache, FetchResult};
use crate::utils::guess_content_type;
use async_trait::async_trait;
use axum::http::header;
use futures::StreamExt;
use reqwest::RequestBuilder;
use tempfile::NamedTempFile;
use tokio::fs;
use tokio::io::{AsyncWriteExt, BufWriter};
use tracing::{info, trace, warn};

#[async_trait]
pub trait RemoteFetcher {
    async fn fetch_from_remote(
        temp_file: NamedTempFile,
        artifact_path: String,
        repo_name: String,
        request: RequestBuilder,
    ) -> Option<FetchResult>;
}

#[async_trait]
impl RemoteFetcher for Cache {
    async fn fetch_from_remote(
        temp_file: NamedTempFile,
        artifact_path: String,
        repo_name: String,
        request: RequestBuilder,
    ) -> Option<FetchResult> {
        match request.send().await {
            Err(e) => {
                warn!("Failed to fetch from {}: {}", repo_name, e);
                return None;
            }
            Ok(response) => {
                let artifact_path = artifact_path.as_str();
                if response.status().is_success() {
                    let content_type = response
                        .headers()
                        .get(header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_else(|| guess_content_type(artifact_path))
                        .to_string();
                    let file = fs::File::create(&temp_file).await;
                    if let Err(e) = file {
                        info!("Unable to create tmp file: {}", e);
                        return None;
                    }
                    // safe, we just checked on Err
                    let mut writer = BufWriter::with_capacity(262_144, file.unwrap());
                    let mut stream = response.bytes_stream();

                    while let Some(chunk) = stream.next().await {
                        match chunk {
                            Err(e) => {
                                info!("Error downloading {}: {}", artifact_path, e);
                                return None;
                            }
                            Ok(chunk) => {
                                trace!(
                                    "partially downloaded {} from {} ({} bytes)",
                                    artifact_path,
                                    repo_name,
                                    chunk.len()
                                );
                                let write_result = writer.write_all(&chunk).await;
                                if let Err(e) = write_result {
                                    info!("Error writing to {}: {}", artifact_path, e);
                                    return None;
                                }
                            }

                        }
                    }
                    if let Err(e) = writer.flush().await {
                        info!("Error flushing to {}: {}", artifact_path, e);
                    }
                    return Some(FetchResult {
                        downloaded_file: temp_file,
                        content_type,
                        repository_name: repo_name,
                    });
                } else {
                    info!(
                        "Repository {} returned status {} for {}",
                        repo_name,
                        response.status(),
                        artifact_path
                    );
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use crate::cache::{Cache, FetchResult};
    use crate::remote_fetcher::{RemoteFetcher, guess_content_type};
    use async_trait::async_trait;
    use bytes::Bytes;
    use reqwest::RequestBuilder;
    use std::error::Error;
    use std::time::Duration;
    use tempfile::NamedTempFile;
    use testcontainers::core::WaitFor;
    use testcontainers::runners::AsyncRunner;
    use testcontainers::{Image, ImageExt};
    use tokio::fs::File;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Clone)]
    struct Nginx {}

    impl Default for Nginx {
        fn default() -> Self {
            Self {}
        }
    }

    impl Image for Nginx {
        fn name(&self) -> &str {
            "nginx"
        }

        fn tag(&self) -> &str {
            "latest"
        }

        fn ready_conditions(&self) -> Vec<WaitFor> {
            vec![WaitFor::Duration {
                length: Duration::from_secs(1),
            }]
        }
    }

    #[tokio::test]
    async fn test_fetch_from_remote_success() -> Result<(), Box<dyn Error>> {
        // Initialize the test container
        let container = Nginx::default();

        // Create a test file in the container
        let test_filename = "test-artifact.jar";
        let test_content = "mock jar file content";

        let container = container.with_copy_to(
            format!("/usr/share/nginx/html/{}", test_filename),
            test_content.as_bytes().to_vec(),
        );

        let container = container.start().await?;

        // Get the container's host port and construct the URL
        let host_port = container.get_host_port_ipv4(80).await?;
        let base_url = format!("http://localhost:{}", host_port);

        // Create a reqwest client
        let client = reqwest::Client::new();
        let request = client.get(format!("{}/{}", base_url, test_filename));

        // Call the RemoteFetcher implementation
        let temp_file = NamedTempFile::new()?;

        let result = Cache::fetch_from_remote(
            temp_file,
            test_filename.to_string(),
            "test-repo".to_string(),
            request,
        )
        .await;

        // Verify the result
        assert!(result.is_some(), "Expected a successful fetch result");
        let fetch_result = result.unwrap();
        assert_eq!(fetch_result.repository_name, "test-repo");
        assert_eq!(fetch_result.content_type, "application/java-archive");
        let mut f = File::open(fetch_result.downloaded_file).await?;
        let mut s = String::new();
        f.read_to_string(&mut s).await.unwrap();
        assert_eq!(s, Bytes::from(test_content));
        Ok(())
    }

    #[tokio::test]
    async fn test_fetch_from_remote_not_found() -> Result<(), Box<dyn Error>> {
        // Initialize the test container
        let container = Nginx::default();

        let container = container.start().await?;

        // Get the container's host port and construct the URL for a non-existent file
        let host_port = container.get_host_port_ipv4(80).await?;
        let base_url = format!("http://localhost:{}", host_port);

        // Create a reqwest client
        let client = reqwest::Client::new();
        let request = client.get(format!("{}/{}", base_url, "non-existent-file.jar"));

        let temp_file = NamedTempFile::new()?;

        // Call the RemoteFetcher implementation
        let result = Cache::fetch_from_remote(
            temp_file,
            "non-existent-file.jar".to_string(),
            "test-repo".to_string(),
            request,
        )
        .await;

        // Verify the result
        assert!(result.is_none(), "Expected None for a non-existent file");

        Ok(())
    }

    #[tokio::test]
    async fn test_fetch_from_remote_with_timeout() -> Result<(), Box<dyn Error>> {
        // Initialize the test container
        let container = Nginx::default();

        // Create a test file that simulates a slow response
        let test_filename = "slow-artifact.jar";
        let test_content = "slow response content";
        let container = container.with_copy_to(
            format!("/usr/share/nginx/html/{}", test_filename),
            test_content.as_bytes().to_vec(),
        );

        let container = container.start().await?;

        // Get the container's host port and construct the URL
        let host_port = container.get_host_port_ipv4(80).await?;
        let base_url = format!("http://localhost:{}", host_port);

        // Create a reqwest client with a very short timeout
        let client = reqwest::Client::new();
        let request = client
            .get(format!("{}/{}", base_url, test_filename))
            .timeout(Duration::new(0, 10)); // Extremely short timeout to force timeout error

        // Call the RemoteFetcher implementation
        let temp_file = NamedTempFile::new()?;

        let result = Cache::fetch_from_remote(
            temp_file,
            test_filename.to_string(),
            "test-repo".to_string(),
            request,
        )
        .await;

        // Verify the result
        assert!(
            result.is_none(),
            "Expected None for a request that times out"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_fetch_from_remote_different_content_types() -> Result<(), Box<dyn Error>> {
        // Initialize the test container
        let container = Nginx::default();

        // Create test files with different extensions
        let test_files = [
            (
                "pom.xml",
                "<project>test pom content</project>".as_bytes().to_vec(),
                "text/xml",
            ),
            (
                "test.txt",
                "abcdef1234567890".as_bytes().to_vec(),
                "text/plain",
            ),
            ("test.jar", vec![1, 2, 3, 4, 5], "application/java-archive"),
        ];

        let container = container.with_copy_to(
            format!("/usr/share/nginx/html/{}", test_files[0].0),
            test_files[0].1.clone(),
        );
        let container = container.with_copy_to(
            format!("/usr/share/nginx/html/{}", test_files[1].0),
            test_files[1].1.clone(),
        );
        let container = container.with_copy_to(
            format!("/usr/share/nginx/html/{}", test_files[2].0),
            test_files[2].1.clone(),
        );

        let container = container.start().await?;

        // Get the container's host port
        let host_port = container.get_host_port_ipv4(80).await?;
        let base_url = format!("http://localhost:{}", host_port);

        for (filename, content, expected_content_type) in test_files.iter() {
            // Create a reqwest client
            let client = reqwest::Client::new();
            let request = client.get(format!("{}/{}", base_url, filename));
            let temp_file = NamedTempFile::new()?;

            // Call the RemoteFetcher implementation
            let result = Cache::fetch_from_remote(
                temp_file,
                filename.to_string(),
                "test-repo".to_string(),
                request,
            )
            .await;

            // Verify the result
            assert!(
                result.is_some(),
                "Expected a successful fetch result for {}",
                filename
            );
            let fetch_result = result.unwrap();
            let mut f = File::open(fetch_result.downloaded_file).await?;
            let mut s = String::new();
            f.read_to_string(&mut s).await.unwrap();

            assert_eq!(s.as_bytes().to_vec(), *content);
            assert_eq!(fetch_result.content_type, *expected_content_type);
        }

        Ok(())
    }

    // Mock implementation for testing
    struct MockRemoteFetcher;

    #[async_trait]
    impl RemoteFetcher for MockRemoteFetcher {
        async fn fetch_from_remote(
            temp_path: NamedTempFile,
            artifact_path: String,
            repo_name: String,
            _request: RequestBuilder,
        ) -> Option<FetchResult> {
            if artifact_path == "existing-artifact.jar" {
                let mut file = File::create(&temp_path).await.unwrap();
                let _ = file.write_all("mock content".as_bytes()).await.unwrap();
                Some(FetchResult {
                    downloaded_file: temp_path,
                    content_type: "application/java-archive".to_string(),
                    repository_name: repo_name,
                })
            } else {
                None
            }
        }
    }

    #[tokio::test]
    async fn test_mock_remote_fetcher() {
        let temp_file = NamedTempFile::new().unwrap();
        // Test the mock implementation
        let result = MockRemoteFetcher::fetch_from_remote(
            temp_file,
            "existing-artifact.jar".to_string(),
            "mock-repo".to_string(),
            reqwest::Client::new().get("http://example.com"),
        )
        .await;

        assert!(result.is_some());
        let fetch_result = result.unwrap();
        let mut f = File::open(fetch_result.downloaded_file).await.unwrap();
        let mut s = String::new();
        f.read_to_string(&mut s).await.unwrap();

        assert_eq!(s, "mock content");
        assert_eq!(fetch_result.repository_name, "mock-repo");
        let temp_file = NamedTempFile::new().unwrap();
        // Test with non-existent artifact
        let result = MockRemoteFetcher::fetch_from_remote(
            temp_file,
            "non-existent.jar".to_string(),
            "mock-repo".to_string(),
            reqwest::Client::new().get("http://example.com"),
        )
        .await;

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_content_type_guessing() {
        assert_eq!(
            guess_content_type("example.jar"),
            "application/java-archive"
        );
        assert_eq!(guess_content_type("example.pom"), "application/xml");
        assert_eq!(guess_content_type("example.sha1"), "text/plain");
        assert_eq!(
            guess_content_type("example.unknown"),
            "application/octet-stream"
        );
    }
}
