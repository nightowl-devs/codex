//! Local implementations of the OpenCode gate tools (bash/edit/glob/grep/
//! read/write), so Codex can execute them itself instead of a proxy having to
//! rewrite tool names and run them out-of-band.
//!
//! The free-tier inference gateway requires these six tools to be present with
//! their exact schemas; because Codex now ships them natively, no cloaking or
//! name restoration is needed on the wire.

use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::gate_tools_spec::create_bash_tool;
use crate::tools::handlers::gate_tools_spec::create_edit_tool;
use crate::tools::handlers::gate_tools_spec::create_glob_tool;
use crate::tools::handlers::gate_tools_spec::create_grep_tool;
use crate::tools::handlers::gate_tools_spec::create_read_tool;
use crate::tools::handlers::gate_tools_spec::create_write_tool;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::items::CommandExecutionItem;
use codex_protocol::items::CommandExecutionStatus;
use codex_protocol::items::FileChangeItem;
use codex_protocol::items::TurnItem;
use codex_protocol::parse_command::ParsedCommand;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::PatchApplyStatus;
use codex_shell_command::parse_command::parse_command;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

const MAX_OUTPUT_BYTES: usize = 128_000;
const DEFAULT_BASH_TIMEOUT_MS: u64 = 120_000;
const MAX_BASH_TIMEOUT_MS: u64 = 600_000;

fn cap(mut text: String) -> String {
    if text.len() > MAX_OUTPUT_BYTES {
        let mut cut = MAX_OUTPUT_BYTES;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n...[truncated]");
    }
    if text.is_empty() {
        return "(no output)".to_string();
    }
    text
}

fn resolve(path: &str, base: &Path) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

/// The session's workspace root, used to resolve relative tool paths instead of
/// the app-server process cwd (which is `/` when launched from the desktop app).
fn default_cwd(invocation: &ToolInvocation) -> PathBuf {
    resolve_tool_environment(&invocation.step_context.environments, None)
        .map(|environment| environment.cwd().to_path_buf())
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn path_uri(path: &Path) -> PathUri {
    PathUri::from_host_native_path(path).unwrap_or_else(|_| {
        PathUri::from_abs_path(
            &codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path("/")
                .expect("root path is absolute"),
        )
    })
}

fn make_unified_diff(old: &str, new: &str) -> String {
    similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .to_string()
}

async fn emit_bash_completed(
    session: &Session,
    turn: &TurnContext,
    call_id: &str,
    command: &[String],
    cwd: &PathUri,
    parsed_cmd: &[ParsedCommand],
    status: CommandExecutionStatus,
    aggregated_output: String,
    exit_code: Option<i32>,
    duration: Duration,
) {
    session
        .emit_turn_item_completed(
            turn,
            TurnItem::CommandExecution(CommandExecutionItem {
                id: call_id.to_string(),
                plugin_id: None,
                script_path: None,
                process_id: None,
                command: command.to_vec(),
                cwd: cwd.clone(),
                parsed_cmd: parsed_cmd.to_vec(),
                source: ExecCommandSource::UnifiedExecStartup,
                interaction_input: None,
                status,
                aggregated_output: Some(aggregated_output),
                exit_code,
                duration: Some(duration),
                sandbox_type: None,
                model_context: None,
            }),
        )
        .await;
}

// ------------------------------------------------------------------- bash

#[derive(Debug, Deserialize)]
struct BashArgs {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    workdir: Option<String>,
}

pub struct BashHandler;

impl ToolExecutor<ToolInvocation> for BashHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("bash")
    }

    fn spec(&self) -> ToolSpec {
        create_bash_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let base = default_cwd(&invocation);
            let ToolPayload::Function { arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "bash handler received unsupported payload".to_string(),
                ));
            };
            let args: BashArgs = parse_arguments(&arguments)?;
            if args.command.trim().is_empty() {
                return Err(FunctionCallError::RespondToModel(
                    "missing command".to_string(),
                ));
            }
            let timeout = args
                .timeout
                .unwrap_or(DEFAULT_BASH_TIMEOUT_MS)
                .min(MAX_BASH_TIMEOUT_MS);
            let cwd = args
                .workdir
                .as_deref()
                .map(|workdir| resolve(workdir, &base))
                .unwrap_or(base);

            let session = invocation.session.clone();
            let turn = invocation.turn.clone();
            let call_id = invocation.call_id.clone();
            let command_vec = vec![
                "zsh".to_string(),
                "-c".to_string(),
                args.command.clone(),
            ];
            let parsed_cmd = parse_command(&command_vec);
            let cwd_uri = path_uri(&cwd);
            let started_at = Instant::now();
            session
                .emit_turn_item_started(
                    turn.as_ref(),
                    &TurnItem::CommandExecution(CommandExecutionItem {
                        id: call_id.clone(),
                        plugin_id: None,
                        script_path: None,
                        process_id: None,
                        command: command_vec.clone(),
                        cwd: cwd_uri.clone(),
                        parsed_cmd: parsed_cmd.clone(),
                        source: ExecCommandSource::UnifiedExecStartup,
                        interaction_input: None,
                        status: CommandExecutionStatus::InProgress,
                        aggregated_output: None,
                        exit_code: None,
                        duration: None,
                        sandbox_type: None,
                        model_context: None,
                    }),
                )
                .await;

            let mut cmd = tokio::process::Command::new("zsh");
            cmd.arg("-c")
                .arg(&args.command)
                .current_dir(&cwd)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);

            let child = match cmd.spawn() {
                Ok(child) => child,
                Err(e) => {
                    let text = format!("error: failed to spawn command: {e}");
                    emit_bash_completed(
                        &session,
                        turn.as_ref(),
                        &call_id,
                        &command_vec,
                        &cwd_uri,
                        &parsed_cmd,
                        CommandExecutionStatus::Failed,
                        text.clone(),
                        None,
                        started_at.elapsed(),
                    )
                    .await;
                    return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                        text,
                        Some(false),
                    )));
                }
            };

            match tokio::time::timeout(
                Duration::from_millis(timeout),
                child.wait_with_output(),
            )
            .await
            {
                Err(_) => {
                    let text = format!("error: command timed out after {timeout}ms");
                    emit_bash_completed(
                        &session,
                        turn.as_ref(),
                        &call_id,
                        &command_vec,
                        &cwd_uri,
                        &parsed_cmd,
                        CommandExecutionStatus::Failed,
                        text.clone(),
                        None,
                        started_at.elapsed(),
                    )
                    .await;
                    Ok(boxed_tool_output(FunctionToolOutput::from_text(
                        text,
                        Some(false),
                    )))
                }
                Ok(Err(e)) => {
                    let text = format!("error: command failed: {e}");
                    emit_bash_completed(
                        &session,
                        turn.as_ref(),
                        &call_id,
                        &command_vec,
                        &cwd_uri,
                        &parsed_cmd,
                        CommandExecutionStatus::Failed,
                        text.clone(),
                        None,
                        started_at.elapsed(),
                    )
                    .await;
                    Ok(boxed_tool_output(FunctionToolOutput::from_text(
                        text,
                        Some(false),
                    )))
                }
                Ok(Ok(o)) => {
                    let success = o.status.success();
                    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
                    let err = String::from_utf8_lossy(&o.stderr);
                    if !err.is_empty() {
                        if !s.is_empty() {
                            s.push('\n');
                        }
                        s.push_str(&err);
                    }
                    if !success {
                        let code = o.status.code().unwrap_or(-1);
                        s.push_str(&format!("\nexit code {code}"));
                    }
                    let output = cap(s);
                    let status = if success {
                        CommandExecutionStatus::Completed
                    } else {
                        CommandExecutionStatus::Failed
                    };
                    emit_bash_completed(
                        &session,
                        turn.as_ref(),
                        &call_id,
                        &command_vec,
                        &cwd_uri,
                        &parsed_cmd,
                        status,
                        output.clone(),
                        o.status.code(),
                        started_at.elapsed(),
                    )
                    .await;
                    Ok(boxed_tool_output(FunctionToolOutput::from_text(
                        output,
                        Some(success),
                    )))
                }
            }
        })
    }
}

impl CoreToolRuntime for BashHandler {}

// ------------------------------------------------------------------- read

#[derive(Debug, Deserialize)]
struct ReadArgs {
    #[serde(alias = "file_path")]
    filePath: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

pub struct ReadHandler;

impl ToolExecutor<ToolInvocation> for ReadHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("read")
    }

    fn spec(&self) -> ToolSpec {
        create_read_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let base = default_cwd(&invocation);
            let ToolPayload::Function { arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "read handler received unsupported payload".to_string(),
                ));
            };
            let args: ReadArgs = parse_arguments(&arguments)?;
            let path = resolve(&args.filePath, &base);

            if path.is_dir() {
                let mut entries: Vec<String> = std::fs::read_dir(&path)
                    .map_err(|e| {
                        FunctionCallError::RespondToModel(format!("error: {e}"))
                    })?
                    .filter_map(|e| e.ok())
                    .map(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        if e.path().is_dir() {
                            format!("{name}/")
                        } else {
                            name
                        }
                    })
                    .collect();
                entries.sort();
                return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                    cap(entries.join("\n")),
                    Some(true),
                )));
            }

            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                        format!("error: {e}"),
                        Some(false),
                    )));
                }
            };
            let lines: Vec<&str> = text.split('\n').collect();
            let offset = args.offset.unwrap_or(1).max(1);
            let limit = args.limit.unwrap_or(2000).clamp(1, 2000);
            let slice = lines
                .iter()
                .skip(offset - 1)
                .take(limit)
                .enumerate()
                .map(|(i, line)| {
                    let mut l = (*line).to_string();
                    if l.chars().count() > 2000 {
                        l = l.chars().take(2000).collect::<String>();
                    }
                    format!("{}: {}", offset + i, l)
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                cap(slice),
                Some(true),
            )))
        })
    }
}

impl CoreToolRuntime for ReadHandler {}

// ------------------------------------------------------------------ write

#[derive(Debug, Deserialize)]
struct WriteArgs {
    content: String,
    #[serde(alias = "file_path")]
    filePath: String,
}

pub struct WriteHandler;

impl ToolExecutor<ToolInvocation> for WriteHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("write")
    }

    fn spec(&self) -> ToolSpec {
        create_write_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let base = default_cwd(&invocation);
            let ToolPayload::Function { arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "write handler received unsupported payload".to_string(),
                ));
            };
            let args: WriteArgs = parse_arguments(&arguments)?;
            let path = resolve(&args.filePath, &base);
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let session = invocation.session.clone();
            let turn = invocation.turn.clone();
            let call_id = invocation.call_id.clone();
            let change = match std::fs::read_to_string(&path) {
                Ok(prev) => FileChange::Update {
                    unified_diff: make_unified_diff(&prev, &args.content),
                    move_path: None,
                },
                Err(_) => FileChange::Add {
                    content: args.content.clone(),
                },
            };
            let mut changes = HashMap::new();
            changes.insert(path.clone(), change);
            session
                .emit_turn_item_started(
                    turn.as_ref(),
                    &TurnItem::FileChange(FileChangeItem {
                        id: call_id.clone(),
                        changes: changes.clone(),
                        status: None,
                        auto_approved: Some(true),
                        stdout: None,
                        stderr: None,
                    }),
                )
                .await;
            match std::fs::write(&path, args.content.as_bytes()) {
                Ok(()) => {
                    let text = format!("wrote {}", path.display());
                    session
                        .emit_turn_item_completed(
                            turn.as_ref(),
                            TurnItem::FileChange(FileChangeItem {
                                id: call_id.clone(),
                                changes,
                                status: Some(PatchApplyStatus::Completed),
                                auto_approved: None,
                                stdout: Some(text.clone()),
                                stderr: Some(String::new()),
                            }),
                        )
                        .await;
                    Ok(boxed_tool_output(FunctionToolOutput::from_text(
                        text,
                        Some(true),
                    )))
                }
                Err(e) => {
                    let text = format!("error: {e}");
                    session
                        .emit_turn_item_completed(
                            turn.as_ref(),
                            TurnItem::FileChange(FileChangeItem {
                                id: call_id.clone(),
                                changes,
                                status: Some(PatchApplyStatus::Failed),
                                auto_approved: None,
                                stdout: None,
                                stderr: Some(text.clone()),
                            }),
                        )
                        .await;
                    Ok(boxed_tool_output(FunctionToolOutput::from_text(
                        text,
                        Some(false),
                    )))
                }
            }
        })
    }
}

impl CoreToolRuntime for WriteHandler {}

// ------------------------------------------------------------------- edit

#[derive(Debug, Deserialize)]
struct EditArgs {
    #[serde(alias = "file_path")]
    filePath: String,
    #[serde(alias = "old_string")]
    oldString: String,
    #[serde(alias = "new_string")]
    newString: String,
    #[serde(default, alias = "replace_all")]
    replaceAll: bool,
}

pub struct EditHandler;

impl ToolExecutor<ToolInvocation> for EditHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("edit")
    }

    fn spec(&self) -> ToolSpec {
        create_edit_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let base = default_cwd(&invocation);
            let ToolPayload::Function { arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "edit handler received unsupported payload".to_string(),
                ));
            };
            let args: EditArgs = parse_arguments(&arguments)?;
            let path = resolve(&args.filePath, &base);
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                        format!("error: {e}"),
                        Some(false),
                    )));
                }
            };
            let matches = text.matches(&args.oldString).count();
            if matches == 0 {
                return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                    format!("error: oldString not found in {}", path.display()),
                    Some(false),
                )));
            }
            if matches > 1 && !args.replaceAll {
                return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                    format!(
                        "error: Found {matches} matches for oldString in {}. Provide more surrounding lines or set replaceAll.",
                        path.display()
                    ),
                    Some(false),
                )));
            }
            let next = if args.replaceAll {
                text.replace(&args.oldString, &args.newString)
            } else {
                text.replacen(&args.oldString, &args.newString, 1)
            };
            let session = invocation.session.clone();
            let turn = invocation.turn.clone();
            let call_id = invocation.call_id.clone();
            let mut changes = HashMap::new();
            changes.insert(
                path.clone(),
                FileChange::Update {
                    unified_diff: make_unified_diff(&text, &next),
                    move_path: None,
                },
            );
            session
                .emit_turn_item_started(
                    turn.as_ref(),
                    &TurnItem::FileChange(FileChangeItem {
                        id: call_id.clone(),
                        changes: changes.clone(),
                        status: None,
                        auto_approved: Some(true),
                        stdout: None,
                        stderr: None,
                    }),
                )
                .await;
            match std::fs::write(&path, next.as_bytes()) {
                Ok(()) => {
                    let msg = format!("edited {}", path.display());
                    session
                        .emit_turn_item_completed(
                            turn.as_ref(),
                            TurnItem::FileChange(FileChangeItem {
                                id: call_id.clone(),
                                changes,
                                status: Some(PatchApplyStatus::Completed),
                                auto_approved: None,
                                stdout: Some(msg.clone()),
                                stderr: Some(String::new()),
                            }),
                        )
                        .await;
                    Ok(boxed_tool_output(FunctionToolOutput::from_text(
                        msg,
                        Some(true),
                    )))
                }
                Err(e) => {
                    let msg = format!("error: {e}");
                    session
                        .emit_turn_item_completed(
                            turn.as_ref(),
                            TurnItem::FileChange(FileChangeItem {
                                id: call_id.clone(),
                                changes,
                                status: Some(PatchApplyStatus::Failed),
                                auto_approved: None,
                                stdout: None,
                                stderr: Some(msg.clone()),
                            }),
                        )
                        .await;
                    Ok(boxed_tool_output(FunctionToolOutput::from_text(
                        msg,
                        Some(false),
                    )))
                }
            }
        })
    }
}

impl CoreToolRuntime for EditHandler {}

// ------------------------------------------------------------ glob / grep

#[derive(Debug, Deserialize)]
struct GlobArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    include: Option<String>,
}

fn run_rg(argv: Vec<String>, cwd: PathBuf) -> (bool, String) {
    match std::process::Command::new("rg")
        .args(&argv)
        .current_dir(&cwd)
        .output()
    {
        Ok(o) if o.status.success() => (true, String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(o) if o.status.code() == Some(1) => (true, String::new()),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr).into_owned();
            (false, err)
        }
        Err(e) => (false, format!("error: {e}")),
    }
}

pub struct GlobHandler;

impl ToolExecutor<ToolInvocation> for GlobHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("glob")
    }

    fn spec(&self) -> ToolSpec {
        create_glob_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let base = default_cwd(&invocation);
            let ToolPayload::Function { arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "glob handler received unsupported payload".to_string(),
                ));
            };
            let args: GlobArgs = parse_arguments(&arguments)?;
            let dir = args
                .path
                .as_deref()
                .filter(|p| !p.is_empty())
                .map(|p| resolve(p, &base))
                .unwrap_or(base);
            let dir_str = dir.to_string_lossy().into_owned();
            // Always pass an explicit path operand so rg never reads stdin.
            let argv = vec![
                "--files".to_string(),
                "-g".to_string(),
                args.pattern.clone(),
                dir_str,
            ];
            let (ok, out) = tokio::task::spawn_blocking(move || run_rg(argv, dir.clone()))
                .await
                .map_err(|e| FunctionCallError::Fatal(format!("glob join error: {e}")))?;
            let text = if out.trim().is_empty() && ok {
                "(no matches)".to_string()
            } else {
                cap(out)
            };
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                text,
                Some(ok),
            )))
        })
    }
}

impl CoreToolRuntime for GlobHandler {}

pub struct GrepHandler;

impl ToolExecutor<ToolInvocation> for GrepHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("grep")
    }

    fn spec(&self) -> ToolSpec {
        create_grep_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let base = default_cwd(&invocation);
            let ToolPayload::Function { arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "grep handler received unsupported payload".to_string(),
                ));
            };
            let args: GrepArgs = parse_arguments(&arguments)?;
            let dir = args
                .path
                .as_deref()
                .filter(|p| !p.is_empty())
                .map(|p| resolve(p, &base))
                .unwrap_or(base);
            let dir_str = dir.to_string_lossy().into_owned();
            let mut argv = vec![
                "-n".to_string(),
                "--no-heading".to_string(),
                args.pattern.clone(),
                dir_str,
            ];
            if let Some(inc) = args.include.as_deref().filter(|s| !s.is_empty()) {
                argv.push("-g".to_string());
                argv.push(inc.to_string());
            }
            let (ok, out) = tokio::task::spawn_blocking(move || run_rg(argv, dir.clone()))
                .await
                .map_err(|e| FunctionCallError::Fatal(format!("grep join error: {e}")))?;
            let text = if out.trim().is_empty() && ok {
                "(no matches)".to_string()
            } else {
                cap(out)
            };
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                text,
                Some(ok),
            )))
        })
    }
}

impl CoreToolRuntime for GrepHandler {}
