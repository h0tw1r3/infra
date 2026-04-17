//! Mock implementations for testing
//!
//! This module provides mock structures and functions for unit testing
//! without requiring actual system calls.

#[allow(dead_code)]
pub mod os_release {
    /// Parse os-release format manually for testing
    pub fn parse(content: &str) -> std::collections::HashMap<String, String> {
        let mut map = std::collections::HashMap::new();
        for line in content.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                let value = value.trim_matches('"').trim_matches('\'').to_string();
                map.insert(key.to_string(), value);
            }
        }
        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_os_release_parsing() {
        let content = r#"
PRETTY_NAME="Debian GNU/Linux 12 (bookworm)"
VERSION_CODENAME=bookworm
ID=debian
"#;
        let parsed = os_release::parse(content);
        assert_eq!(
            parsed.get("VERSION_CODENAME"),
            Some(&"bookworm".to_string())
        );
        assert_eq!(parsed.get("ID"), Some(&"debian".to_string()));
    }
}
