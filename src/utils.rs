use std::path::{Component, PathBuf};

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
}
