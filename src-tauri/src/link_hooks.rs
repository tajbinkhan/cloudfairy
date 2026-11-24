//! Hands a tunnel's new public link to other tools: writes it to a file and runs a command.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use tauri::{AppHandle, Manager};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::Command,
};

use crate::{settings::TunnelConfig, AppState};

/// How long the command may run before Cloudfairy kills it.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// Output lines from one command run that go to the log view. The rest are dropped so a
/// chatty command can't push cloudflared's own lines out of the log.
const MAX_OUTPUT_LINES: usize = 200;

/// True when `url` only has characters that are safe to paste into a shell command or a
/// dotenv line: `A-Z a-z 0-9 . : / -`.
pub fn is_safe_url(url: &str) -> bool {
    !url.is_empty()
        && url
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '/' | '-'))
}

/// True for names like `PUBLIC_URL`: a letter or `_`, then letters, digits or `_`.
pub fn is_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Sets `name=value` in dotenv-style text. Every line that assigns `name` (with or without
/// `export `) gets the new value; other lines and line endings stay as they are. When no line
/// assigns `name`, a new one is added at the end.
pub fn update_dotenv(content: &str, name: &str, value: &str) -> String {
    let newline = if content.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(content.len() + name.len() + value.len() + 2);
    let mut found = false;
    for line in content.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        let ending = &line[body.len()..];
        match assignment_prefix(body, name) {
            Some(prefix) => {
                found = true;
                out.push_str(&body[..prefix]);
                out.push_str(value);
                out.push_str(ending);
            }
            None => out.push_str(line),
        }
    }
    if !found {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push_str(newline);
        }
        out.push_str(name);
        out.push('=');
        out.push_str(value);
        out.push_str(newline);
    }
    out
}

/// If `line` assigns `name`, the length of the part up to and including the `=`.
fn assignment_prefix(line: &str, name: &str) -> Option<usize> {
    let rest = line.trim_start();
    let rest = rest
        .strip_prefix("export")
        .filter(|r| r.starts_with([' ', '\t']))
        .map_or(rest, str::trim_start);
    let after = rest.strip_prefix(name)?.trim_start_matches([' ', '\t']);
    // Spaces after the `=` stay, so `NAME = value` keeps its look.
    let value = after.strip_prefix('=')?.trim_start_matches([' ', '\t']);
    Some(line.len() - value.len())
}

/// Writes the link to `path`: as a `name=url` line when `var` is set, else as the whole file.
fn write_file(path: &Path, var: &str, url: &str) -> Result<(), String> {
    let content = if var.is_empty() {
        url.to_string()
    } else {
        let old = match std::fs::read(path) {
            Ok(bytes) => String::from_utf8(bytes)
                .map_err(|_| "it isn't a text file, so Cloudfairy left it alone".to_string())?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.to_string()),
        };
        update_dotenv(&old, var, url)
    };
    std::fs::write(path, content).map_err(|e| e.to_string())
}

/// Runs the file and command steps for a new link. Returns a message for the card when a
/// step fails. Called once per new link, never twice at once for the same tunnel.
pub async fn run(app: &AppHandle, cfg: &TunnelConfig, url: &str) -> Result<(), String> {
    let hooks = &cfg.lifecycle;
    if !is_safe_url(url) {
        let msg = "The new link has unexpected characters, so it wasn't passed to the file or command.";
        log(app, &cfg.id, format!("[cloudfairy] {msg}"));
        return Err(msg.into());
    }
    let mut errors = Vec::new();

    let file = hooks.url_file.trim();
    if !file.is_empty() {
        let path = Path::new(file).to_path_buf();
        let var = hooks.url_file_var.trim().to_string();
        let link = url.to_string();
        let result = tauri::async_runtime::spawn_blocking(move || write_file(&path, &var, &link))
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
        match result {
            Ok(()) => log(app, &cfg.id, format!("[cloudfairy] Wrote the link to {file}.")),
            Err(e) => {
                log(app, &cfg.id, format!("[cloudfairy] Couldn't write the link to {file}: {e}"));
                errors.push(format!("Couldn't write the link to {file}."));
            }
        }
    }

    let command = hooks.url_command.trim();
    if !command.is_empty() {
        let (log_app, log_id) = (app.clone(), cfg.id.clone());
        let logger: Logger = Arc::new(move |line| log(&log_app, &log_id, line));
        let home = app.path().home_dir().ok();
        if let Err(e) = run_command(logger, command, url, home, COMMAND_TIMEOUT).await {
            log(app, &cfg.id, format!("[cloudfairy] {e}"));
            errors.push(e);
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!("{} See Logs.", errors.join(" ")))
    }
}

fn log(app: &AppHandle, id: &str, line: String) {
    app.state::<AppState>().tunnels.log_line(app, id, line);
}

/// Where a command's output goes: the tunnel's log view, or a list in tests.
type Logger = Arc<dyn Fn(String) + Send + Sync>;

/// Runs `command` in the shell with the link in `{url}` and `CLOUDFAIRY_URL`. Its output
/// goes to `log`. Fails on a non-zero exit, or when it runs longer than `timeout`.
async fn run_command(
    log: Logger,
    command: &str,
    url: &str,
    cwd: Option<PathBuf>,
    timeout: Duration,
) -> Result<(), String> {
    let command = command.replace("{url}", url);
    log(format!("[cloudfairy] Running: {command}"));

    let mut cmd = shell(&command);
    cmd.env("CLOUDFAIRY_URL", url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    crate::tunnel::hide_console(&mut cmd);
    // Its own process group, so a timeout can stop everything the command started.
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd.spawn().map_err(|e| format!("Couldn't run the command: {e}"))?;
    let lines = Arc::new(AtomicUsize::new(0));
    let pumps: Vec<_> = [
        child.stdout.take().map(|s| Box::new(s) as Box<dyn AsyncRead + Unpin + Send>),
        child.stderr.take().map(|s| Box::new(s) as Box<dyn AsyncRead + Unpin + Send>),
    ]
    .into_iter()
    .flatten()
    .map(|reader| tauri::async_runtime::spawn(pump(log.clone(), lines.clone(), reader)))
    .collect();

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => return Err(format!("Couldn't wait for the command: {e}")),
        Err(_) => {
            kill_tree(child.id());
            let _ = child.kill().await;
            return Err(format!(
                "The command was still running after {} s, so Cloudfairy stopped it.",
                timeout.as_secs()
            ));
        }
    };
    // Give the readers a moment to log the last lines before the exit code.
    for pump in pumps {
        let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
    }
    match status.code() {
        Some(0) => {
            log("[cloudfairy] The command finished with exit code 0.".into());
            Ok(())
        }
        Some(code) => Err(format!("The command failed with exit code {code}.")),
        None => Err(format!("The command ended without an exit code ({status}).")),
    }
}

#[cfg(windows)]
fn shell(command: &str) -> Command {
    let mut cmd = Command::new("cmd");
    // With /S, cmd drops the outer quotes and runs the rest exactly as typed.
    cmd.args(["/D", "/S", "/C"]).raw_arg(format!("\"{command}\""));
    cmd
}

#[cfg(not(windows))]
fn shell(command: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd
}

/// Stops the command and anything it started. Killing only the shell would leave its
/// children running.
fn kill_tree(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let mut taskkill = std::process::Command::new("taskkill");
        taskkill.args(["/T", "/F", "/PID", &pid.to_string()]);
        taskkill.stdout(Stdio::null()).stderr(Stdio::null());
        std::os::windows::process::CommandExt::creation_flags(&mut taskkill, 0x0800_0000);
        let _ = taskkill.status();
    }
}

/// Copies the command's output to `log`, line by line. `lines` counts the lines of stdout
/// and stderr together.
async fn pump(log: Logger, lines: Arc<AtomicUsize>, reader: Box<dyn AsyncRead + Unpin + Send>) {
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let count = lines.fetch_add(1, Ordering::Relaxed) + 1;
        if count > MAX_OUTPUT_LINES {
            continue;
        }
        let text = String::from_utf8_lossy(&buf);
        log(format!("[command] {}", text.trim_end_matches(['\r', '\n'])));
        if count == MAX_OUTPUT_LINES {
            log(format!(
                "[cloudfairy] The command printed more. Only the first {MAX_OUTPUT_LINES} lines are shown."
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://maple-ranges-quietly-orbit.trycloudflare.com";

    #[test]
    fn checks_urls() {
        assert!(is_safe_url(URL));
        assert!(is_safe_url("http://127.0.0.1:8080/app"));
        assert!(!is_safe_url(""));
        assert!(!is_safe_url("https://x.trycloudflare.com; rm -rf ~"));
        assert!(!is_safe_url("https://x.trycloudflare.com/$(id)"));
        assert!(!is_safe_url("https://x.trycloudflare.com/a&b"));
        assert!(!is_safe_url("https://x.trycloudflare.com/`id`"));
        assert!(!is_safe_url("https://x.trycloudflare.com\nEVIL=1"));
        assert!(!is_safe_url("https://x.trycloudflare.com/a b"));
        assert!(!is_safe_url("https://x.trycloudflare.com/a\"b"));
        assert!(!is_safe_url("https://x.trycloudflare.com/%41"));
    }

    #[test]
    fn checks_var_names() {
        assert!(is_var_name("PUBLIC_URL"));
        assert!(is_var_name("_x1"));
        assert!(!is_var_name(""));
        assert!(!is_var_name("1URL"));
        assert!(!is_var_name("PUBLIC-URL"));
        assert!(!is_var_name("A B"));
    }

    #[test]
    fn dotenv_adds_to_empty_file() {
        assert_eq!(update_dotenv("", "PUBLIC_URL", URL), format!("PUBLIC_URL={URL}\n"));
    }

    #[test]
    fn dotenv_replaces_value_and_keeps_other_lines() {
        let old = "# comment\nPORT=3000\nPUBLIC_URL=https://old.trycloudflare.com\nDEBUG=1\n";
        let new = format!("# comment\nPORT=3000\nPUBLIC_URL={URL}\nDEBUG=1\n");
        assert_eq!(update_dotenv(old, "PUBLIC_URL", URL), new);
    }

    #[test]
    fn dotenv_keeps_crlf_and_missing_final_newline() {
        let old = "PORT=3000\r\nPUBLIC_URL=old\r\nDEBUG=1";
        assert_eq!(
            update_dotenv(old, "PUBLIC_URL", URL),
            format!("PORT=3000\r\nPUBLIC_URL={URL}\r\nDEBUG=1")
        );
        // A new line uses the file's line ending, after closing the last line.
        assert_eq!(
            update_dotenv("PORT=3000\r\nDEBUG=1", "PUBLIC_URL", URL),
            format!("PORT=3000\r\nDEBUG=1\r\nPUBLIC_URL={URL}\r\n")
        );
        assert_eq!(update_dotenv("PORT=3000", "PUBLIC_URL", URL), format!("PORT=3000\nPUBLIC_URL={URL}\n"));
    }

    #[test]
    fn dotenv_matches_whole_names_only() {
        let old = "PUBLIC_URL_OLD=a\n# PUBLIC_URL=b\nMY_PUBLIC_URL=c\n";
        assert_eq!(
            update_dotenv(old, "PUBLIC_URL", URL),
            format!("{old}PUBLIC_URL={URL}\n")
        );
    }

    #[test]
    fn dotenv_keeps_export_and_spacing() {
        let old = "export PUBLIC_URL=old\n  PUBLIC_URL = old\nexportPUBLIC_URL=x\n";
        assert_eq!(
            update_dotenv(old, "PUBLIC_URL", URL),
            format!("export PUBLIC_URL={URL}\n  PUBLIC_URL = {URL}\nexportPUBLIC_URL=x\n")
        );
    }

    /// Runs `command` and returns its result and log lines.
    fn run_in_shell(command: &str, timeout: Duration) -> (Result<(), String>, Vec<String>) {
        let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = lines.clone();
        let logger: Logger = Arc::new(move |l| sink.lock().unwrap().push(l));
        let result = tauri::async_runtime::block_on(run_command(logger, command, URL, None, timeout));
        let lines = lines.lock().unwrap().clone();
        (result, lines)
    }

    #[test]
    fn runs_commands_with_the_link() {
        let command = if cfg!(windows) {
            r#"echo arg={url} && echo env=%CLOUDFAIRY_URL% && echo "quoted text""#
        } else {
            r#"echo arg={url} && echo env=$CLOUDFAIRY_URL && echo "quoted text""#
        };
        let (result, lines) = run_in_shell(command, Duration::from_secs(20));
        assert_eq!(result, Ok(()), "{lines:?}");
        let has = |s: &str| lines.iter().any(|l| l.starts_with("[command] ") && l.contains(s));
        assert!(has(&format!("arg={URL}")), "{lines:?}");
        assert!(has(&format!("env={URL}")), "{lines:?}");
        assert!(has("quoted text"), "{lines:?}");
        assert!(lines.last().unwrap().contains("exit code 0"), "{lines:?}");
    }

    #[test]
    fn reports_failures_and_timeouts() {
        let (result, _) = run_in_shell("exit 3", Duration::from_secs(20));
        assert_eq!(result.unwrap_err(), "The command failed with exit code 3.");

        let slow = if cfg!(windows) { "ping -n 30 127.0.0.1 > nul" } else { "sleep 30" };
        let started = std::time::Instant::now();
        let (result, _) = run_in_shell(slow, Duration::from_secs(1));
        assert!(result.unwrap_err().contains("still running after 1 s"));
        assert!(started.elapsed() < Duration::from_secs(10), "the command is stopped");
    }

    #[test]
    fn writes_files() {
        let dir = std::env::temp_dir().join(format!("cloudfairy-hooks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let env = dir.join(".env");
        let _ = std::fs::remove_file(&env);

        write_file(&env, "PUBLIC_URL", URL).unwrap();
        assert_eq!(std::fs::read_to_string(&env).unwrap(), format!("PUBLIC_URL={URL}\n"));
        std::fs::write(&env, "A=1\r\nPUBLIC_URL=old\r\n").unwrap();
        write_file(&env, "PUBLIC_URL", URL).unwrap();
        assert_eq!(std::fs::read_to_string(&env).unwrap(), format!("A=1\r\nPUBLIC_URL={URL}\r\n"));

        let bare = dir.join("url.txt");
        write_file(&bare, "", URL).unwrap();
        assert_eq!(std::fs::read_to_string(&bare).unwrap(), URL);

        std::fs::write(&env, [0xff, 0xfe, 0x00]).unwrap();
        assert!(write_file(&env, "PUBLIC_URL", URL).is_err(), "binary files are left alone");
        assert_eq!(std::fs::read(&env).unwrap(), [0xff, 0xfe, 0x00]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
