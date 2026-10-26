use super::manager::CodexAuth;
use super::manager::ExternalAuth;
use super::manager::ExternalAuthFuture;
use super::manager::ExternalAuthRefreshContext;
use codex_protocol::config_types::ModelProviderAuthInfo;
use std::fmt;
use std::io;
use std::ops::Deref;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Instant;
use tokio::process::Command;
use tokio::sync::Mutex;

#[derive(Clone)]
pub(crate) struct BearerTokenRefresher {
    state: Arc<ExternalBearerAuthState>,
}

impl BearerTokenRefresher {
    pub(crate) fn new(config: ModelProviderAuthInfo) -> Self {
        Self {
            state: Arc::new(ExternalBearerAuthState::new(config)),
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "external bearer cache misses intentionally hold cached_token across the provider command to avoid duplicate refreshes"
    )]
    async fn resolve(&self) -> io::Result<CodexAuth> {
        let access_token = {
            let mut cached = self.state.cached_token.lock().await;
            if let Some(cached_token) = cached.as_ref() {
                let should_use_cached_token = match self.state.config.refresh_interval() {
                    Some(refresh_interval) => cached_token.fetched_at.elapsed() < refresh_interval,
                    None => true,
                };
                if should_use_cached_token {
                    return Ok(CodexAuth::from_api_key(cached_token.access_token.as_str()));
                }
            }

            let access_token = run_provider_auth_command(&self.state.config).await?;
            *cached = Some(CachedExternalBearerToken {
                access_token: access_token.clone(),
                fetched_at: Instant::now(),
            });
            access_token
        };
        Ok(CodexAuth::from_api_key(access_token.as_str()))
    }

    async fn refresh(&self, _context: ExternalAuthRefreshContext) -> io::Result<CodexAuth> {
        let access_token = run_provider_auth_command(&self.state.config).await?;
        let mut cached = self.state.cached_token.lock().await;
        *cached = Some(CachedExternalBearerToken {
            access_token: access_token.clone(),
            fetched_at: Instant::now(),
        });
        Ok(CodexAuth::from_api_key(access_token.as_str()))
    }
}

impl ExternalAuth for BearerTokenRefresher {
    fn resolve(&self) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(BearerTokenRefresher::resolve(self))
    }

    fn refresh(&self, context: ExternalAuthRefreshContext) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(BearerTokenRefresher::refresh(self, context))
    }
}

impl fmt::Debug for BearerTokenRefresher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BearerTokenRefresher")
            .finish_non_exhaustive()
    }
}

struct ExternalBearerAuthState {
    config: ModelProviderAuthInfo,
    cached_token: Mutex<Option<CachedExternalBearerToken>>,
}

impl ExternalBearerAuthState {
    fn new(config: ModelProviderAuthInfo) -> Self {
        Self {
            config,
            cached_token: Mutex::new(None),
        }
    }
}

struct CachedExternalBearerToken {
    access_token: String,
    fetched_at: Instant,
}

async fn run_provider_auth_command(config: &ModelProviderAuthInfo) -> io::Result<String> {
    let program = resolve_provider_auth_program(&config.command, &config.cwd)?;
    let mut command = Command::from(codex_utils_process::background_command(&program));
    command
        .args(config.args.iter().map(Deref::deref))
        .current_dir(config.cwd.as_path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let output = tokio::time::timeout(config.timeout(), command.output())
        .await
        .map_err(|_| {
            io::Error::other(format!(
                "provider auth command `{}` timed out after {} ms",
                config.command,
                config.timeout_ms.get()
            ))
        })?
        .map_err(|err| {
            io::Error::other(format!(
                "provider auth command `{}` failed to start: {err}",
                config.command
            ))
        })?;

    if !output.status.success() {
        let status = output.status;
        // Helpers may print credentials to either stream on failure. Neither
        // stream is safe to propagate into diagnostics or tracing.
        return Err(io::Error::other(format!(
            "provider auth command `{}` exited with status {status}",
            config.command
        )));
    }

    let stdout = String::from_utf8(output.stdout).map_err(|_| {
        io::Error::other(format!(
            "provider auth command `{}` wrote non-UTF-8 data to stdout",
            config.command
        ))
    })?;
    let access_token = stdout.trim().to_string();
    if access_token.is_empty() {
        return Err(io::Error::other(format!(
            "provider auth command `{}` produced an empty token",
            config.command
        )));
    }

    Ok(access_token)
}

fn resolve_provider_auth_program(command: &str, cwd: &Path) -> io::Result<PathBuf> {
    let path = Path::new(command);
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }

    if path.components().count() > 1 {
        return Ok(cwd.join(path));
    }

    Ok(PathBuf::from(command))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn helper(home: &Path, mode: &str) -> ModelProviderAuthInfo {
        #[cfg(unix)]
        let (command, args) = {
            let path = home.join("helper.sh");
            std::fs::write(
                &path,
                r#"case "$1" in
success) printf synthetic-helper-secret ;;
empty) printf '  \n' ;;
failure) printf synthetic-helper-secret; printf synthetic-helper-secret >&2; exit 7 ;;
wait) touch started; while [ ! -f release ]; do sleep 0.01; done; printf synthetic-helper-secret ;;
esac
"#,
            )
            .unwrap();
            (
                "sh",
                vec![path.to_string_lossy().into_owned(), mode.to_string()],
            )
        };
        #[cfg(windows)]
        let (command, args) = {
            let path = home.join("helper.cmd");
            std::fs::write(&path, concat!(
                "@echo off\r\n",
                "if \"%~1\"==\"success\" (echo synthetic-helper-secret& exit /b 0)\r\n",
                "if \"%~1\"==\"empty\" exit /b 0\r\n",
                "if \"%~1\"==\"failure\" (echo synthetic-helper-secret& echo synthetic-helper-secret 1>&2& exit /b 7)\r\n",
                "echo started>started\r\n:wait\r\nif not exist release goto wait\r\necho synthetic-helper-secret\r\n",
            )).unwrap();
            (
                "cmd.exe",
                vec![
                    "/D".into(),
                    "/C".into(),
                    path.to_string_lossy().into_owned(),
                    mode.to_string(),
                ],
            )
        };
        serde_json::from_value(json!({
            "command": command, "args": args, "cwd": home,
            "timeout_ms": 5_000, "refresh_interval_ms": 60_000,
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn helper_success_empty_and_nonzero_keep_output_out_of_diagnostics() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(
            run_provider_auth_command(&helper(home.path(), "success"))
                .await
                .unwrap(),
            "synthetic-helper-secret"
        );
        for (mode, expected) in [
            ("empty", "produced an empty token"),
            ("failure", "exited with status"),
        ] {
            let error = run_provider_auth_command(&helper(home.path(), mode))
                .await
                .unwrap_err();
            assert!(error.to_string().contains(expected));
            assert!(!error.to_string().contains("synthetic-helper-secret"));
            assert!(!format!("{error:?}").contains("synthetic-helper-secret"));
        }
    }

    #[tokio::test]
    async fn helper_timeout_and_cancellation_release_cached_auth_lock() {
        let home = tempfile::tempdir().unwrap();
        let mut config = helper(home.path(), "wait");
        config.timeout_ms = std::num::NonZeroU64::new(100).unwrap();
        let error = run_provider_auth_command(&config).await.unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(!error.to_string().contains("synthetic-helper-secret"));
        let _ = std::fs::remove_file(home.path().join("started"));
        let refresher = BearerTokenRefresher::new(helper(home.path(), "wait"));
        {
            let pending = refresher.resolve();
            tokio::pin!(pending);
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                tokio::select! {
                    result = &mut pending => panic!("helper must wait: {result:?}"),
                    () = async {
                        while !home.path().join("started").exists() { tokio::task::yield_now().await; }
                    } => {}
                }
            }).await.unwrap();
            // Dropping the unresolved future is the cancellation path.
        }
        std::fs::write(home.path().join("release"), []).unwrap();
        let token = tokio::time::timeout(std::time::Duration::from_secs(5), refresher.resolve())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(token.api_key(), Some("synthetic-helper-secret"));
    }
}
