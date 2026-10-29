use std::collections::HashMap;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::future::Future;
use std::io::ErrorKind;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::Path;
#[cfg(not(unix))]
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::time::Duration;
use std::time::Instant;

use async_channel::Sender;
use codex_protocol::shell_environment::is_non_inheritable_env_var;
#[cfg(unix)]
use codex_utils_pty::Command;
#[cfg(windows)]
use codex_utils_pty::JobObject;
use futures::future::try_join;
use tokio::io::AsyncWriteExt;
#[cfg(not(unix))]
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::timeout;
use tracing::Span;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

use super::CommandShell;
use super::ConfiguredHandler;
use super::ConfiguredHandlerKind;
use super::HandlerRunResult;
use super::dispatcher::ParsedHandler;
use super::dispatcher::hook_event_name_label;
use super::dispatcher::hook_execution_mode_label;
use super::dispatcher::hook_handler_type_label;
use super::dispatcher::hook_scope_label;
use super::dispatcher::hook_source_label;
use super::dispatcher::scope_for_event;
use crate::output_spill::HookOutputSpiller;
use codex_protocol::ThreadId;
use codex_protocol::protocol::HookCompletedEvent;
use codex_protocol::protocol::HookHandlerType;
use codex_protocol::protocol::HookOutputEntry;
use codex_protocol::protocol::HookOutputEntryKind;

const MAX_CONCURRENT_ASYNC_HOOKS: usize = 8;

/// Owns command execution and bounded asynchronous work for one session.
#[derive(Clone)]
pub(crate) struct CommandHookRuntime {
    shell: CommandShell,
    environment: Arc<Vec<(OsString, OsString)>>,
    result_sender: Sender<HookCompletedEvent>,
    state: Arc<Mutex<CommandHookRuntimeState>>,
    output_spiller: HookOutputSpiller,
}

struct CommandHookRuntimeState {
    concurrency_limit: Arc<Semaphore>,
    tasks: JoinSet<()>,
}

impl Default for CommandHookRuntimeState {
    fn default() -> Self {
        Self {
            concurrency_limit: Arc::new(Semaphore::new(MAX_CONCURRENT_ASYNC_HOOKS)),
            tasks: JoinSet::new(),
        }
    }
}

impl CommandHookRuntime {
    pub(crate) fn new(
        shell: CommandShell,
        environment: Arc<Vec<(OsString, OsString)>>,
        thread_id: ThreadId,
        result_sender: Sender<HookCompletedEvent>,
    ) -> Self {
        Self {
            shell,
            environment,
            result_sender,
            state: Arc::new(Mutex::new(CommandHookRuntimeState::default())),
            output_spiller: HookOutputSpiller::new(thread_id),
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, CommandHookRuntimeState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn reconfigured(&self, shell: CommandShell) -> Self {
        Self {
            shell,
            environment: Arc::clone(&self.environment),
            result_sender: self.result_sender.clone(),
            state: Arc::clone(&self.state),
            output_spiller: self.output_spiller.clone(),
        }
    }

    pub(crate) fn output_spiller(&self) -> &HookOutputSpiller {
        &self.output_spiller
    }

    pub(crate) fn schedule_async_hook<T: 'static>(
        &self,
        handler: ConfiguredHandler,
        input_json: String,
        cwd: std::path::PathBuf,
        turn_id: Option<String>,
        parse: fn(&ConfiguredHandler, HandlerRunResult, Option<String>) -> ParsedHandler<T>,
    ) {
        if self.result_sender.is_closed() {
            return;
        }

        let result_sender = self.result_sender.clone();
        let runtime = self.clone();
        self.schedule_async_task(async move {
            let result = match &handler.kind {
                ConfiguredHandlerKind::Command { command, env, .. } => {
                    run_command(&runtime, &handler, command, env, &input_json, &cwd).await
                }
                ConfiguredHandlerKind::McpTool { .. } => return,
            };
            let mut hook_result = parse(&handler, result, turn_id).completed;
            let mut entries = Vec::new();
            let mut warnings = Vec::new();

            for entry in std::mem::take(&mut hook_result.run.entries) {
                match entry.kind {
                    HookOutputEntryKind::Context => {
                        let text = runtime
                            .output_spiller
                            .maybe_spill_text_with_limit(
                                entry.text,
                                handler.additional_context_limit,
                            )
                            .await;
                        entries.push(HookOutputEntry {
                            kind: HookOutputEntryKind::Context,
                            text,
                        });
                    }
                    HookOutputEntryKind::Warning => warnings.push(entry),
                    HookOutputEntryKind::Error => entries.push(entry),
                    HookOutputEntryKind::Stop | HookOutputEntryKind::Feedback => {}
                }
            }

            entries.extend(warnings);
            hook_result.run.entries = entries;
            let _ = result_sender.try_send(hook_result);
        });
    }

    pub(crate) fn schedule_async_task(&self, task: impl Future<Output = ()> + Send + 'static) {
        let mut state = self.lock_state();
        if state.concurrency_limit.is_closed() {
            return;
        }

        while state.tasks.try_join_next().is_some() {}
        let concurrency_limit = Arc::clone(&state.concurrency_limit);
        state.tasks.spawn(async move {
            let Ok(_permit) = concurrency_limit.acquire_owned().await else {
                return;
            };
            task.await;
        });
    }

    pub(crate) async fn shutdown(&self) {
        let mut tasks = {
            let mut state = self.lock_state();
            state.concurrency_limit.close();
            std::mem::take(&mut state.tasks)
        };
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
}

#[tracing::instrument(
    name = "codex.hooks.command",
    level = "trace",
    skip_all,
    fields(
        hook.event_name = hook_event_name_label(handler.event_name),
        hook.handler_type = hook_handler_type_label(HookHandlerType::Command),
        hook.execution_mode = hook_execution_mode_label(handler.execution_mode()),
        hook.scope = hook_scope_label(scope_for_event(handler.event_name)),
        hook.source = hook_source_label(handler.source),
        hook.display_order = handler.display_order,
        hook.timeout_sec = handler.timeout_sec,
        hook.command_outcome = tracing::field::Empty,
    )
)]
pub(crate) async fn run_command(
    runtime: &CommandHookRuntime,
    handler: &ConfiguredHandler,
    command_line: &str,
    env: &HashMap<String, String>,
    input_json: &str,
    cwd: &Path,
) -> HandlerRunResult {
    let started_at = chrono::Utc::now().timestamp();
    let started = Instant::now();
    let timeout_started = tokio::time::Instant::now();
    let duration = Duration::from_secs(handler.timeout_sec);
    if let Some(trust) = &handler.plugin_trust {
        let error = match timeout(duration, trust.verify()).await {
            Ok(Ok(())) => None,
            Ok(Err(err)) => Some(err.to_string()),
            Err(_) => Some(format!("hook timed out after {}s", handler.timeout_sec)),
        };
        if let Some(error) = error {
            return finish_command_run(
                started_at,
                started,
                CommandRunCompletion {
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    error: Some(error),
                    outcome: "trust_error",
                },
            );
        }
    }

    let mut command = build_command(&runtime.shell, command_line, &runtime.environment, env);
    command.current_dir(cwd);
    #[cfg(windows)]
    let mut queue_diagnostic =
        QueueHookDiagnostic::selected(&runtime.shell, command_line, &command, started);

    #[cfg(windows)]
    let (child, process_tree_job) = match JobObject::spawn_background(&mut command) {
        Ok((child, job)) => (Ok(child), job),
        Err(error) => (Err(error), None),
    };
    #[cfg(not(windows))]
    let child = command.spawn();

    let mut child = match child {
        Ok(child) => child,
        Err(err) => {
            #[cfg(windows)]
            if let Some(diagnostic) = queue_diagnostic.as_mut() {
                diagnostic.spawn_error(err.raw_os_error());
            }
            return finish_command_run(
                started_at,
                started,
                CommandRunCompletion {
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    error: Some(err.to_string()),
                    outcome: "spawn_error",
                },
            );
        }
    };
    #[cfg(windows)]
    if let Some(diagnostic) = queue_diagnostic.as_mut() {
        diagnostic.record(serde_json::json!({"stage":"spawned"}));
    }

    let mut process_tree_guard = ProcessTreeGuard {
        process_id: child.id(),
        #[cfg(windows)]
        job: process_tree_job,
    };

    let stdin = child.stdin.take();
    let write_stdin = async {
        if let Some(mut stdin) = stdin
            && let Err(err) = stdin.write_all(input_json.as_bytes()).await
            && err.kind() != ErrorKind::BrokenPipe
        {
            return Err(("stdin_error", format!("failed to write hook stdin: {err}")));
        }
        Ok(())
    };
    let wait_with_output = async {
        child
            .wait_with_output()
            .await
            .map_err(|err| ("wait_error", err.to_string()))
    };

    // Drain output while sending input so neither pipe can block the other, and
    // include stdin writes in the deadline even when the hook never reads them.
    let result = match timeout(
        duration.saturating_sub(timeout_started.elapsed()),
        try_join(write_stdin, wait_with_output),
    )
    .await
    {
        Ok(Ok(((), output))) => {
            // Successfully completed hooks may intentionally leave detached helpers running.
            #[cfg(windows)]
            if let Some(job) = process_tree_guard.job.as_ref() {
                let _ = job.preserve_descendants();
            }
            process_tree_guard.process_id = None;
            finish_command_run(
                started_at,
                started,
                CommandRunCompletion {
                    exit_code: output.status.code(),
                    stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                    stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                    error: None,
                    outcome: "completed",
                },
            )
        }
        Ok(Err((outcome, error))) => finish_command_run(
            started_at,
            started,
            CommandRunCompletion {
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                error: Some(error),
                outcome,
            },
        ),
        Err(_) => finish_command_run(
            started_at,
            started,
            CommandRunCompletion {
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                error: Some(format!("hook timed out after {}s", handler.timeout_sec)),
                outcome: "timeout",
            },
        ),
    };
    drop(process_tree_guard);
    #[cfg(windows)]
    if let Some(diagnostic) = queue_diagnostic.as_mut() {
        diagnostic.completed(&result);
    }
    result
}

// Temporary, diagnostic-branch-only observer. Never merge into accepted product source.
#[cfg(windows)]
struct QueueHookDiagnostic {
    path: std::path::PathBuf,
    started: Instant,
    finished: bool,
}

#[cfg(windows)]
impl QueueHookDiagnostic {
    fn digest(bytes: &[u8]) -> String {
        use sha2::Digest;
        format!("{:x}", sha2::Sha256::digest(bytes))
    }

    fn selected(
        shell: &CommandShell,
        command_line: &str,
        command: &Command,
        started: Instant,
    ) -> Option<Self> {
        use std::os::windows::ffi::OsStrExt;
        static LAUNCHES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        if std::env::var("CODEX_QUEUE_DIAGNOSTIC_NONCE").as_deref() != Ok("6c4e9a72") {
            return None;
        }
        // hooks.json has no per-command env field. Recognize only this exact
        // nonce-bound fixture command and script; leave command execution intact.
        let script = std::path::PathBuf::from(command_line.strip_prefix("python3 ")?);
        let home = script.parent()?;
        let log = home.join("queue_prompt_hook.log");
        if script.file_name()? != "queue_prompt_hook.py"
            || log != home.join("queue_prompt_hook.log")
            || command_line != format!("python3 {}", script.display())
            || !script.is_absolute()
            || !std::fs::symlink_metadata(&script)
                .ok()?
                .file_type()
                .is_file()
            || script.canonicalize().ok()?.parent()? != home.canonicalize().ok()?
        {
            return None;
        }
        let source = std::fs::read_to_string(&script).ok()?;
        let log_text = log.to_str()?;
        if source.len() > 4096 || source.matches(log_text).count() != 1 {
            return None;
        }
        let normalized = source.replace(log_text, "<TEST_LOG>");
        if Self::digest(normalized.as_bytes())
            != "55b7d7d5a6a4827d7b8475920bc813779b5197b1318430468df4fda7f5e76e18"
        {
            return None;
        }
        if LAUNCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 8 {
            eprintln!("QUEUE_HOOK_DIAGNOSTIC launch_bound_exceeded");
            return None;
        }
        let path = home.join("queue_hook_diagnostic_6c4e9a72.jsonl");
        if std::fs::symlink_metadata(&path)
            .is_ok_and(|metadata| !metadata.is_file() || metadata.file_type().is_symlink())
        {
            return None;
        }
        let observer = Self {
            path,
            started,
            finished: false,
        };
        let actual = command.as_std();
        let program_bytes = actual
            .get_program()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let basename = Path::new(actual.get_program())
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        let program_kind = match basename.as_str() {
            "cmd" | "cmd.exe" => "cmd",
            "pwsh" | "pwsh.exe" => "pwsh",
            "powershell" | "powershell.exe" => "powershell",
            "sh" | "sh.exe" | "bash" | "bash.exe" => "sh",
            _ => "other",
        };
        let mut flag_mask = 0_u32;
        let mut unknown_args = 0_usize;
        let mut command_arg = false;
        let mut argc = 0_usize;
        for argument in actual.get_args() {
            argc += 1;
            let value = argument.to_string_lossy();
            let bit = match value.to_ascii_lowercase().as_str() {
                "/c" => 1,
                "/d" => 2,
                "/s" => 4,
                "-c" => 8,
                "-command" => 16,
                "-noprofile" => 32,
                "-noninteractive" => 64,
                _ => 0,
            };
            flag_mask |= bit;
            if value == command_line || value == format!("\"{command_line}\"") {
                command_arg = true;
            } else if bit == 0 {
                unknown_args += 1;
            }
        }
        // Observe only the exact configured child environment after env_clear.
        // Values and paths are never recorded; Windows keys are case-insensitive.
        let path_present = actual.get_envs().any(|(key, value)| {
            key.to_str()
                .is_some_and(|key| key.eq_ignore_ascii_case("PATH"))
                && value.is_some()
        });
        let pathext = actual.get_envs().find_map(|(key, value)| {
            key.to_str()
                .is_some_and(|key| key.eq_ignore_ascii_case("PATHEXT"))
                .then_some(value)
                .flatten()
        });
        let extensions = pathext
            .map(|value| {
                value
                    .to_string_lossy()
                    .split(';')
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let contains_extension = |expected: &str| {
            extensions
                .iter()
                .any(|extension| extension.eq_ignore_ascii_case(expected))
        };
        observer.record(serde_json::json!({
            "path_present":path_present,"pathext_present":pathext.is_some(),
            "pathext_empty":pathext.is_some_and(|value| value.is_empty()),
            "pathext_extension_count":extensions.iter().filter(|extension| !extension.is_empty()).count(),
            "pathext_has_exe":contains_extension(".EXE"),"pathext_has_com":contains_extension(".COM"),
            "pathext_has_bat":contains_extension(".BAT"),"pathext_has_cmd":contains_extension(".CMD"),
            "stage":"built", "shell_source":if shell.program.is_empty() {"fallback"} else {"explicit"},
            "program_kind":program_kind,"program_path_sha256":Self::digest(&program_bytes),
            "argc":argc,"arg_flag_mask":flag_mask,"unknown_args":unknown_args,"exact_command_arg":command_arg,
            "command_sha256":Self::digest(command_line.as_bytes()),"script_sha256":Self::digest(source.as_bytes()),
            "normalized_script_sha256":Self::digest(normalized.as_bytes()),"script_has_space":script.to_string_lossy().contains(char::is_whitespace),
            "script_arg_quoted":false,"script_regular":true
        }));
        Some(observer)
    }

    fn record(&self, mut value: serde_json::Value) {
        use std::io::Write;
        value["nonce"] = serde_json::json!("6c4e9a72");
        value["elapsed_ms"] =
            serde_json::json!(self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64);
        let Ok(mut line) = serde_json::to_vec(&value) else {
            return;
        };
        if line.len() > 2048 {
            return;
        }
        line.push(b'\n');
        let current = std::fs::symlink_metadata(&self.path);
        if current
            .as_ref()
            .is_ok_and(|m| !m.is_file() || m.file_type().is_symlink() || m.len() > 30720)
        {
            eprintln!("QUEUE_HOOK_DIAGNOSTIC record_bound_or_identity_rejected");
            return;
        }
        let file = if current.is_err() {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&self.path)
        } else {
            std::fs::OpenOptions::new().append(true).open(&self.path)
        };
        if file.and_then(|mut f| f.write_all(&line)).is_err() {
            eprintln!("QUEUE_HOOK_DIAGNOSTIC record_write_failed");
        }
    }

    fn spawn_error(&mut self, os_error: Option<i32>) {
        self.record(serde_json::json!({"stage":"spawn_error","raw_os_error":os_error}));
        self.finished = true;
    }

    fn completed(&mut self, result: &HandlerRunResult) {
        let lower = result
            .stderr
            .chars()
            .take(4096)
            .collect::<String>()
            .to_ascii_lowercase();
        let classification = if lower.contains("commandnotfoundexception") {
            "powershell_command_not_found"
        } else if lower.contains("not recognized") || lower.contains("not found") {
            "command_not_found"
        } else if lower.contains("can't open file") || lower.contains("cannot open file") {
            "python_cannot_open_file"
        } else if lower.contains("jsondecodeerror") {
            "json_decode"
        } else if lower.contains("permissionerror") || lower.contains("permission denied") {
            "permission_denied"
        } else if lower.is_empty() {
            "empty"
        } else {
            "other"
        };
        let mut entry = false;
        let mut decoded = false;
        let mut appended = false;
        let mut executable_hash = None;
        let mut version = [None; 3];
        for line in result.stderr.lines().take(64) {
            let Some(json) = line.strip_prefix("QUEUE_HOOK_DIAG ") else {
                continue;
            };
            if json.len() > 1024 {
                continue;
            }
            let Ok(marker) = serde_json::from_str::<serde_json::Value>(json) else {
                continue;
            };
            if marker["nonce"].as_str() != Some("6c4e9a72") {
                continue;
            }
            match marker["phase"].as_str() {
                Some("entry") => entry = true,
                Some("decoded") => decoded = true,
                Some("log_appended") => appended = true,
                _ => continue,
            }
            if let Some(hash) = marker["executable_path_sha256"]
                .as_str()
                .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
            {
                executable_hash = Some(hash.to_owned());
            }
            for (index, component) in version.iter_mut().enumerate() {
                *component = marker["version"][index].as_u64().filter(|n| *n < 10000);
            }
        }
        let error_kind = result.error.as_deref().map(|error| {
            if error.starts_with("hook timed out after ") {
                "timeout"
            } else if error.starts_with("failed to write hook stdin:") {
                "stdin_error"
            } else {
                "other"
            }
        });
        self.record(serde_json::json!({"stage":"completed","exit_code":result.exit_code,"error_kind":error_kind,
            "stdout_bytes":result.stdout.len(),"stdout_sha256":Self::digest(result.stdout.as_bytes()),
            "stderr_bytes":result.stderr.len(),"stderr_sha256":Self::digest(result.stderr.as_bytes()),"stderr_class":classification,
            "python_entry":entry,"python_decoded":decoded,"python_log_appended":appended,
            "python_executable_path_sha256":executable_hash,"python_major":version[0],"python_minor":version[1],"python_micro":version[2]}));
        self.finished = true;
    }
}

#[cfg(windows)]
impl Drop for QueueHookDiagnostic {
    fn drop(&mut self) {
        if !self.finished {
            self.record(serde_json::json!({"stage":"cancelled_before_completion"}));
        }
    }
}

// Needed only until command hooks move to the exec server, which owns process-tree cleanup.
struct ProcessTreeGuard {
    process_id: Option<u32>,
    #[cfg(windows)]
    job: Option<JobObject>,
}

impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        let Some(process_id) = self.process_id else {
            return;
        };

        #[cfg(unix)]
        {
            let _ = codex_utils_pty::process_group::kill_process_group(process_id);
        }

        #[cfg(windows)]
        {
            if let Some(job) = self.job.as_ref() {
                let _ = job.terminate();
            } else {
                let _ = std::process::Command::new("taskkill")
                    .args(["/PID", &process_id.to_string(), "/T", "/F"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .creation_flags(CREATE_NO_WINDOW)
                    .spawn();
            }
        }
    }
}

struct CommandRunCompletion {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    error: Option<String>,
    outcome: &'static str,
}

fn finish_command_run(
    started_at: i64,
    started: Instant,
    completion: CommandRunCompletion,
) -> HandlerRunResult {
    Span::current().record("hook.command_outcome", completion.outcome);
    HandlerRunResult {
        context_metadata: codex_protocol::models::ContentItemMetadata::command_hook(),
        started_at,
        completed_at: chrono::Utc::now().timestamp(),
        duration_ms: started.elapsed().as_millis().try_into().unwrap_or(i64::MAX),
        exit_code: completion.exit_code,
        stdout: completion.stdout,
        stderr: completion.stderr,
        error: completion.error,
    }
}

fn build_command(
    shell: &CommandShell,
    command_line: &str,
    environment: &[(OsString, OsString)],
    env: &HashMap<String, String>,
) -> Command {
    let mut command = if shell.program.is_empty() {
        Command::new(default_shell_program(environment))
    } else {
        Command::new(&shell.program)
    };
    if shell.program.is_empty() {
        #[cfg(windows)]
        command.arg("/C");
        #[cfg(not(windows))]
        command.arg("-lc");
    } else {
        command.args(&shell.args);
    }
    #[cfg(windows)]
    if shell.program.is_empty() || shell.args.iter().any(|arg| arg.eq_ignore_ascii_case("/c")) {
        command.raw_arg(format!(r#""{command_line}""#));
    } else {
        command.arg(command_line);
    }
    #[cfg(not(windows))]
    command.arg(command_line);

    #[cfg(unix)]
    command.process_mode(codex_utils_pty::ProcessMode::NewSession);
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    #[cfg(not(unix))]
    command
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // Both launchers start with an empty environment. Replay the session snapshot
    // before hook overrides, filtering restricted names from both sources.
    command.envs(
        environment
            .iter()
            .map(|(key, value)| (key.as_os_str(), value.as_os_str()))
            .chain(
                env.iter()
                    .map(|(key, value)| (OsStr::new(key), OsStr::new(value))),
            )
            .filter(|(key, _)| !key.to_str().is_some_and(is_non_inheritable_env_var)),
    );
    command
}

fn default_shell_program(environment: &[(OsString, OsString)]) -> OsString {
    #[cfg(windows)]
    let (environment_variable, fallback_program) = ("COMSPEC", "cmd.exe");

    #[cfg(not(windows))]
    let (environment_variable, fallback_program) = ("SHELL", "/bin/sh");

    environment
        .iter()
        .find(|(key, _)| {
            #[cfg(windows)]
            {
                key.to_str()
                    .is_some_and(|key| key.eq_ignore_ascii_case(environment_variable))
            }

            #[cfg(not(windows))]
            {
                key == OsStr::new(environment_variable)
            }
        })
        .map(|(_, value)| value.clone())
        .unwrap_or_else(|| OsString::from(fallback_program))
}

#[cfg(test)]
#[path = "command_runner_tests.rs"]
mod tests;
