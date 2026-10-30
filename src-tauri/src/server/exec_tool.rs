use super::AppState;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use walkdir::WalkDir;

const MAX_OUTPUT_BYTES: usize = 128 * 1024;
const MAX_SOURCE_BYTES: usize = 2 * 1024 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 32 * 1024 * 1024;

pub struct ExecReport {
    pub success: bool,
    pub content: String,
    pub metadata: Value,
}

struct RunOutput {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
    timed_out: bool,
    output_truncated: bool,
}

struct PreparedRuntime {
    program: PathBuf,
    args: Vec<String>,
    artifact_name: String,
    compile_to: Option<PathBuf>,
}

/// 在回合隔离目录中运行用户显式提交的源码。
///
/// 这里不会回退到应用内捆绑的 Python、Node 或编译器。所需运行时必须已经
/// 存在于用户系统的 `PATH` 中。环境变量会清空，只保留进程运行和便携路径
/// 所需的白名单，避免把模型密钥泄露给外部程序。
pub async fn execute(
    state: &AppState,
    turn_id: &str,
    arguments: &Value,
) -> Result<ExecReport, String> {
    let code = string_arg(arguments, "code");
    if code.is_empty() {
        return Err("exec 需要非空 code。".to_string());
    }
    if code.len() > MAX_SOURCE_BYTES {
        return Err(format!(
            "源码不能超过 {} MiB。",
            MAX_SOURCE_BYTES / 1024 / 1024
        ));
    }
    reject_dangerous_code(&code)?;
    let language = arguments
        .get("language")
        .and_then(Value::as_str)
        .unwrap_or("python")
        .trim()
        .to_ascii_lowercase();
    let timeout_seconds = arguments
        .get("timeout")
        .and_then(Value::as_u64)
        .unwrap_or(30)
        .clamp(1, 300);
    let stdin = arguments
        .get("stdin")
        .and_then(Value::as_str)
        .unwrap_or("")
        .as_bytes()
        .to_vec();

    let workspace = workspace_path(state, turn_id)?;
    fs::create_dir_all(&workspace).map_err(|error| format!("无法创建执行工作目录：{error}"))?;
    let mut prepared = prepare_runtime(&workspace, &language, &code)?;
    if let Some(executable) = prepared.compile_to.clone() {
        let mut compile = Command::new(&prepared.program);
        compile.args(&prepared.args);
        let compiled = run_command(
            &mut compile,
            state,
            &workspace,
            None,
            Duration::from_secs(120),
        )
        .await?;
        if compiled.timed_out || compiled.exit_code != Some(0) || !executable.is_file() {
            let content = format!(
                "编译失败。\n编译器：{}\n退出码：{}\n\nSTDOUT\n```text\n{}\n```\n\nSTDERR\n```text\n{}\n```",
                prepared.program.display(),
                compiled
                    .exit_code
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "未知".to_string()),
                compiled.stdout.trim_end(),
                compiled.stderr.trim_end()
            );
            return Ok(ExecReport {
                success: false,
                content,
                metadata: json!({
                    "kind": "exec_compile_error",
                    "language": language,
                    "compiler": prepared.program.to_string_lossy(),
                    "stdout": compiled.stdout,
                    "stderr": compiled.stderr,
                    "exit_code": compiled.exit_code,
                    "timed_out": compiled.timed_out,
                    "artifacts": collect_artifacts(&workspace)
                }),
            });
        }
        prepared.program = executable;
        prepared.args.clear();
    }
    let mut command = Command::new(&prepared.program);
    command.args(&prepared.args);
    let output = run_command(
        &mut command,
        state,
        &workspace,
        Some(&stdin),
        Duration::from_secs(timeout_seconds),
    )
    .await?;

    let artifacts = collect_artifacts(&workspace);
    let success = !output.timed_out && output.exit_code == Some(0);
    let status = if output.timed_out {
        format!("执行超过 {timeout_seconds} 秒，已终止。")
    } else {
        format!(
            "退出码：{}",
            output
                .exit_code
                .map(|value| value.to_string())
                .unwrap_or_else(|| "未知".to_string())
        )
    };
    let content = format!(
        "语言：{language}\n运行时：{}\n{status}\n\nSTDOUT\n```text\n{}\n```\n\nSTDERR\n```text\n{}\n```",
        prepared.program.display(),
        output.stdout.trim_end(),
        output.stderr.trim_end()
    );
    let metadata = json!({
        "kind": "exec",
        "language": language,
        "runtime": prepared.program.to_string_lossy(),
        "workspace": workspace.to_string_lossy(),
        "entry_artifact": prepared.artifact_name,
        "stdout": output.stdout,
        "stderr": output.stderr,
        "exit_code": output.exit_code,
        "timed_out": output.timed_out,
        "output_truncated": output.output_truncated,
        "artifacts": artifacts
    });
    Ok(ExecReport {
        success,
        content,
        metadata,
    })
}

fn prepare_runtime(
    workspace: &Path,
    language: &str,
    code: &str,
) -> Result<PreparedRuntime, String> {
    match language {
        "python" => {
            let script = workspace.join("main.py");
            fs::write(&script, code).map_err(|error| format!("无法写入 Python 源码：{error}"))?;
            let (program, mut args) = resolve_python()?;
            args.push(path_string(&script));
            Ok(PreparedRuntime {
                program,
                args,
                artifact_name: "main.py".to_string(),
                compile_to: None,
            })
        }
        "shell" => {
            #[cfg(windows)]
            {
                let script = workspace.join("main.ps1");
                fs::write(&script, code)
                    .map_err(|error| format!("无法写入 PowerShell 脚本：{error}"))?;
                let program = find_program(&["pwsh.exe", "powershell.exe"]).ok_or_else(|| {
                    "系统中未找到 PowerShell，请先安装后再使用 shell 执行。".to_string()
                })?;
                Ok(PreparedRuntime {
                    program,
                    args: vec![
                        "-NoLogo".to_string(),
                        "-NoProfile".to_string(),
                        "-NonInteractive".to_string(),
                        "-ExecutionPolicy".to_string(),
                        "Bypass".to_string(),
                        "-File".to_string(),
                        path_string(&script),
                    ],
                    artifact_name: "main.ps1".to_string(),
                    compile_to: None,
                })
            }
            #[cfg(not(windows))]
            {
                let script = workspace.join("main.sh");
                fs::write(&script, code)
                    .map_err(|error| format!("无法写入 shell 脚本：{error}"))?;
                let program = find_program(&["sh", "/bin/sh"])
                    .ok_or_else(|| "系统中未找到 sh。".to_string())?;
                Ok(PreparedRuntime {
                    program,
                    args: vec![path_string(&script)],
                    artifact_name: "main.sh".to_string(),
                    compile_to: None,
                })
            }
        }
        "c" | "cpp" => {
            let (extension, compiler_names) = if language == "c" {
                ("c", &["gcc", "clang"][..])
            } else {
                ("cpp", &["g++", "clang++"][..])
            };
            let source = workspace.join(format!("main.{extension}"));
            fs::write(&source, code).map_err(|error| format!("无法写入 C/C++ 源码：{error}"))?;
            let compiler = find_program(compiler_names).ok_or_else(|| {
                format!(
                    "系统中未找到 {} 编译器。Exec 不会下载或捆绑编译器。",
                    compiler_names.join(" / ")
                )
            })?;
            let executable = workspace.join(if cfg!(windows) { "main.exe" } else { "main" });
            let entry = executable
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("main")
                .to_string();
            Ok(PreparedRuntime {
                program: compiler,
                args: vec![
                    "-O2".to_string(),
                    path_string(&source),
                    "-o".to_string(),
                    path_string(&executable),
                ],
                artifact_name: entry,
                compile_to: Some(executable),
            })
        }
        _ => Err("language 必须是 python、c、cpp 或 shell。".to_string()),
    }
}

fn resolve_python() -> Result<(PathBuf, Vec<String>), String> {
    if let Some(program) = find_program(&["python.exe", "python3.exe", "python", "python3"]) {
        return Ok((program, Vec::new()));
    }
    #[cfg(windows)]
    if let Some(program) = find_program(&["py.exe", "py"]) {
        return Ok((program, vec!["-3".to_string()]));
    }
    Err("系统中未找到 Python。Exec 只调用用户自行安装的解释器，不会回退到随包运行时。".to_string())
}

async fn run_command(
    command: &mut Command,
    state: &AppState,
    workspace: &Path,
    stdin: Option<&[u8]>,
    limit: Duration,
) -> Result<RunOutput, String> {
    command.current_dir(workspace);
    clear_environment(command);
    set_safe_environment(command);
    for (key, value) in state.paths.environment() {
        command.env(key, value);
    }
    // 外部程序只能看到系统运行时与安装目录内的 HOME/TEMP，不会继承模型密钥。
    for key in [
        "PATH",
        "PATHEXT",
        "SYSTEMROOT",
        "WINDIR",
        "COMSPEC",
        "OS",
        "PROCESSOR_ARCHITECTURE",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    run_child(command, stdin, limit).await
}

fn clear_environment(command: &mut Command) {
    command.env_clear();
}

fn set_safe_environment(command: &mut Command) {
    command.env("NO_COLOR", "1");
}

async fn run_child(
    command: &mut Command,
    stdin: Option<&[u8]>,
    limit: Duration,
) -> Result<RunOutput, String> {
    let mut child = command
        .spawn()
        .map_err(|error| format!("无法启动外部程序：{error}"))?;
    if let Some(mut pipe) = child.stdin.take() {
        if let Some(input) = stdin {
            pipe.write_all(input)
                .await
                .map_err(|error| format!("无法写入程序标准输入：{error}"))?;
        }
        let _ = pipe.shutdown().await;
    }
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "无法读取程序标准输出。".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "无法读取程序标准错误。".to_string())?;
    let mut stdout_task = tokio::spawn(read_limited(stdout));
    let mut stderr_task = tokio::spawn(read_limited(stderr));
    let waited = timeout(limit, child.wait()).await;
    let (status, timed_out) = match waited {
        Ok(result) => (
            Some(result.map_err(|error| format!("等待外部程序失败：{error}"))?),
            false,
        ),
        Err(_) => {
            #[cfg(windows)]
            if let Some(pid) = child.id() {
                let _ = Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/T", "/F"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await;
            }
            let _ = child.kill().await;
            let status = child
                .wait()
                .await
                .map_err(|error| format!("终止超时程序失败：{error}"))?;
            (Some(status), true)
        }
    };
    let (stdout, stdout_truncated) = collect_reader(&mut stdout_task).await;
    let (stderr, stderr_truncated) = collect_reader(&mut stderr_task).await;
    Ok(RunOutput {
        stdout,
        stderr,
        exit_code: status.and_then(|value| value.code()),
        timed_out,
        output_truncated: stdout_truncated || stderr_truncated,
    })
}

async fn collect_reader(task: &mut tokio::task::JoinHandle<(String, bool)>) -> (String, bool) {
    match timeout(Duration::from_secs(2), &mut *task).await {
        Ok(Ok(value)) => value,
        Ok(Err(_)) => ("读取输出任务异常结束。".to_string(), true),
        Err(_) => {
            task.abort();
            ("输出流在进程结束后仍未关闭，已停止读取。".to_string(), true)
        }
    }
}

async fn read_limited(mut reader: impl AsyncRead + Unpin) -> (String, bool) {
    let mut output = Vec::new();
    let mut truncated = false;
    let mut buffer = [0u8; 16 * 1024];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => {
                if output.len() < MAX_OUTPUT_BYTES {
                    let keep = read.min(MAX_OUTPUT_BYTES - output.len());
                    output.extend_from_slice(&buffer[..keep]);
                    truncated |= keep < read;
                } else {
                    truncated = true;
                }
            }
            Err(error) => {
                output.extend_from_slice(format!("\n读取输出失败：{error}").as_bytes());
                truncated = true;
                break;
            }
        }
    }
    (String::from_utf8_lossy(&output).into_owned(), truncated)
}

fn workspace_path(state: &AppState, turn_id: &str) -> Result<PathBuf, String> {
    let safe = turn_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let safe = if safe.trim_matches('_').is_empty() {
        format!("manual-{}", uuid::Uuid::new_v4().simple())
    } else {
        safe
    };
    Ok(state.paths.data.join("exec_workspaces").join(safe))
}

fn collect_artifacts(workspace: &Path) -> Vec<Value> {
    let mut artifacts = WalkDir::new(workspace)
        .max_depth(3)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            if metadata.len() > MAX_ARTIFACT_BYTES {
                return Some(json!({
                    "name": entry.file_name().to_string_lossy(),
                    "relative_path": entry.path().strip_prefix(workspace).ok()?.to_string_lossy(),
                    "size_bytes": metadata.len(),
                    "sha256": Value::Null,
                    "hash_skipped": "文件超过 32 MiB"
                }));
            }
            let digest = fs::read(entry.path()).ok().map(|bytes| {
                let mut hasher = Sha256::new();
                hasher.update(bytes);
                hex::encode(hasher.finalize())
            });
            Some(json!({
                "name": entry.file_name().to_string_lossy(),
                "relative_path": entry.path().strip_prefix(workspace).ok()?.to_string_lossy(),
                "size_bytes": metadata.len(),
                "sha256": digest
            }))
        })
        .take(100)
        .collect::<Vec<_>>();
    artifacts.sort_by(|left, right| {
        left.get("relative_path")
            .and_then(Value::as_str)
            .cmp(&right.get("relative_path").and_then(Value::as_str))
    });
    artifacts
}

fn reject_dangerous_code(code: &str) -> Result<(), String> {
    let normalized = code
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let blocked = [
        "rm -rf",
        "rm -fr",
        "del /f",
        "del /q",
        "rmdir /s",
        "rd /s",
        "remove-item -recurse",
        "format-volume",
        "format.com",
        "mkfs",
        "diskpart",
        "dd if=",
        "shutdown ",
        "shutdown.exe",
        "reboot ",
        "poweroff",
    ];
    if let Some(pattern) = blocked
        .iter()
        .find(|pattern| normalized.contains(**pattern))
    {
        return Err(format!("Exec 拒绝可能破坏宿主机文件的命令片段：{pattern}"));
    }
    Ok(())
}

fn find_program(candidates: &[&str]) -> Option<PathBuf> {
    for candidate in candidates {
        let path = Path::new(candidate);
        if path.components().count() > 1 {
            if path.is_file() {
                return Some(path.to_path_buf());
            }
            continue;
        }
        let Some(paths) = std::env::var_os("PATH") else {
            continue;
        };
        for directory in std::env::split_paths(&paths) {
            let direct = directory.join(candidate);
            if direct.is_file() {
                return Some(direct);
            }
            #[cfg(windows)]
            for extension in std::env::var("PATHEXT")
                .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
                .split(';')
            {
                let path = directory.join(format!("{candidate}{extension}"));
                if path.is_file() {
                    return Some(path);
                }
            }
        }
    }
    None
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn string_arg(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string()
}
