/// Read a key from ~/.werma/.env file.
/// Falls back to VarError::NotPresent if not found.
pub fn read_env_file_key(key: &str) -> Result<String, std::env::VarError> {
    let env_path = dirs::home_dir()
        .map(|h| h.join(".werma/.env"))
        .unwrap_or_default();

    read_env_key_from_path(&env_path, key)
}

/// Read a key from a specific .env file path.
fn read_env_key_from_path(path: &std::path::Path, key: &str) -> Result<String, std::env::VarError> {
    if let Ok(content) = std::fs::read_to_string(path) {
        for line in content.lines() {
            let line = line.trim();
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            if let Some((k, v)) = line.split_once('=')
                && k.trim() == key
            {
                return Ok(v.trim().trim_matches('"').trim_matches('\'').to_string());
            }
        }
    }
    Err(std::env::VarError::NotPresent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_key_value() {
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join(".env");
        std::fs::write(&env_file, "FOO=bar\nBAZ=qux\n").unwrap();

        assert_eq!(read_env_key_from_path(&env_file, "FOO").unwrap(), "bar");
        assert_eq!(read_env_key_from_path(&env_file, "BAZ").unwrap(), "qux");
    }

    #[test]
    fn parse_quoted_values() {
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join(".env");
        std::fs::write(&env_file, "A=\"hello world\"\nB='single quoted'\n").unwrap();

        assert_eq!(
            read_env_key_from_path(&env_file, "A").unwrap(),
            "hello world"
        );
        assert_eq!(
            read_env_key_from_path(&env_file, "B").unwrap(),
            "single quoted"
        );
    }

    #[test]
    fn skip_comments_and_empty_lines() {
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join(".env");
        std::fs::write(&env_file, "# comment\n\nKEY=value\n  # another comment\n").unwrap();

        assert_eq!(read_env_key_from_path(&env_file, "KEY").unwrap(), "value");
        assert!(read_env_key_from_path(&env_file, "comment").is_err());
    }

    #[test]
    fn missing_key_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join(".env");
        std::fs::write(&env_file, "FOO=bar\n").unwrap();

        let result = read_env_key_from_path(&env_file, "MISSING");
        assert!(result.is_err());
    }

    #[test]
    fn missing_file_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join("nonexistent");

        let result = read_env_key_from_path(&env_file, "ANY");
        assert!(result.is_err());
    }

    #[test]
    fn handles_whitespace_around_key_and_value() {
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join(".env");
        std::fs::write(&env_file, "  KEY  =  value  \n").unwrap();

        assert_eq!(read_env_key_from_path(&env_file, "KEY").unwrap(), "value");
    }
}
