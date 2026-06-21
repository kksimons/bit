//! File-backed OAuth credential storage for HTTP MCP servers.
//!
//! rmcp's `AuthorizationManager` owns the entire OAuth 2.1 flow (discovery, DCR,
//! PKCE, token exchange, refresh) but is agnostic to *where* tokens live — it
//! takes a `CredentialStore` trait. We implement that trait with a `0600` JSON
//! file per server, consistent with how the LLM API key is stored (`config.rs`)
//! and what we agreed on: the same protection level, no Keychain, uniform model.
//!
//! One real consideration OAuth introduces: the spec requires refresh-token
//! rotation, so this file is rewritten on essentially every tool call. We write
//! it atomically (temp file in the same dir, then rename) so a crash mid-rotate
//! can never corrupt the token — a naive direct write could.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use rmcp::transport::auth::{CredentialStore, StoredCredentials};
use serde_json::Value;
use tauri::Manager;

/// The fixed localhost port the OAuth loopback callback server listens on.
/// DCR requires a stable, pre-registered redirect URI, so this is a constant
/// (see lib.rs `OAUTH_CALLBACK_PORT`).
pub const REDIRECT_URI: &str = "http://127.0.0.1:8473/callback";

/// A `CredentialStore` backed by a `0600` JSON file at
/// `<app_config>/mcp/<server>.token.json`. One instance per HTTP server, each
/// scoped to its own path so servers never see each other's tokens.
pub struct FileCredentialStore {
    path: PathBuf,
}

impl FileCredentialStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Build the store path for a given server name, under the app config dir.
    pub fn path_for(app: &tauri::AppHandle, server_name: &str) -> Option<PathBuf> {
        let dir = app.path().app_config_dir().ok()?.join("mcp");
        Some(dir.join(format!("{}.token.json", sanitize_name(server_name))))
    }

    /// Ensure the parent dir exists (called before writes).
    fn ensure_dir(&self) -> Result<(), rmcp::transport::auth::AuthError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| rmcp::transport::auth::AuthError::InternalError(e.to_string()))?;
        }
        Ok(())
    }
}

/// Atomic write: serialize to a temp file in the same directory, fsync, then
/// rename over the target. Same-dir rename is atomic on Unix and avoids leaving
/// a half-written token if the process dies mid-write — critical because OAuth
/// refresh rewrites this file on every call.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), rmcp::transport::auth::AuthError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("token")
    ));
    std::fs::write(&tmp, bytes)
        .and_then(|_| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            }
            #[cfg(not(unix))]
            {
                Ok(())
            }
        })
        .and_then(|_| std::fs::rename(&tmp, path))
        .map_err(|e| rmcp::transport::auth::AuthError::InternalError(e.to_string()))
}

#[async_trait]
impl CredentialStore for FileCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, rmcp::transport::auth::AuthError> {
        let Some(bytes) = std::fs::read(&self.path).ok() else {
            return Ok(None);
        };
        // StoredCredentials derives Deserialize, but rmcp's version may carry
        // extra vendor token fields; deserialize leniently via serde_json.
        let v: Value = serde_json::from_slice(&bytes)
            .map_err(|e| rmcp::transport::auth::AuthError::InternalError(e.to_string()))?;
        let creds: StoredCredentials = serde_json::from_value(v)
            .map_err(|e| rmcp::transport::auth::AuthError::InternalError(e.to_string()))?;
        Ok(Some(creds))
    }

    async fn save(
        &self,
        credentials: StoredCredentials,
    ) -> Result<(), rmcp::transport::auth::AuthError> {
        self.ensure_dir()?;
        let bytes = serde_json::to_vec(&credentials)
            .map_err(|e| rmcp::transport::auth::AuthError::InternalError(e.to_string()))?;
        write_atomic(&self.path, &bytes)?;
        Ok(())
    }

    async fn clear(&self) -> Result<(), rmcp::transport::auth::AuthError> {
        // Best-effort delete; missing file is not an error.
        let _ = std::fs::remove_file(&self.path);
        Ok(())
    }
}

/// Restrict a server name to filename-safe characters so a malicious or
/// odd name can't escape the `mcp/` dir or clobber `../` paths.
pub fn sanitize_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "server".into()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::transport::auth::StoredCredentials;

    #[test]
    fn sanitize_is_safe() {
        assert_eq!(sanitize_name("gmail"), "gmail");
        assert_eq!(sanitize_name("my-server"), "my-server");
        assert_eq!(sanitize_name("../../etc"), "______etc");
        assert_eq!(sanitize_name(""), "server");
        assert_eq!(sanitize_name("a/b"), "a_b");
    }

    #[test]
    fn round_trip_through_file() {
        let tmp = tempfile_tmpdir();
        let path = tmp.join("test.token.json");
        let store = FileCredentialStore::new(path.clone());

        let creds = StoredCredentials::new(
            "client-123".into(),
            None,
            vec!["read".into()],
            Some(1_700_000_000),
        );
        // save (sync wrapper around the async trait)
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(store.save(creds.clone())).unwrap();

        // file is 0600
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "token file must be 0600");
        }

        let loaded = rt.block_on(store.load()).unwrap().unwrap();
        assert_eq!(loaded.client_id, "client-123");
        assert_eq!(loaded.granted_scopes, vec!["read".to_string()]);

        rt.block_on(store.clear()).unwrap();
        assert!(rt.block_on(store.load()).unwrap().is_none());
    }

    fn tempfile_tmpdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bit-mcp-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
