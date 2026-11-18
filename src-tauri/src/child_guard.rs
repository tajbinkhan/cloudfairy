//! Makes sure cloudflared doesn't outlive Cloudfairy, even when Cloudfairy crashes or is
//! force-quit (`kill_on_drop` and the exit handler only cover a normal quit).
//!
//! - Linux: the kernel kills each child when Cloudfairy dies (`PR_SET_PDEATHSIG`).
//! - Windows: each child joins a job object that is killed when Cloudfairy's handle to it
//!   closes, which happens however the process ends.
//! - Unix (the only protection on macOS, a backup on Linux): running PIDs are written to a
//!   file, and anything left over from a crashed session is killed on the next launch.

use std::io;

use tokio::process::{Child, Command};

/// Call once at startup, before any tunnel starts.
pub fn init(data_dir: &std::path::Path) {
    #[cfg(unix)]
    pid_file::init(data_dir.join("cloudflared-pids.txt"));
    #[cfg(not(unix))]
    let _ = data_dir;
}

/// Spawns `cmd` with the platform's protection. `command_line` is the program and its
/// arguments joined by spaces; it identifies the process when cleaning up after a crash.
pub async fn spawn(cmd: Command, command_line: String) -> io::Result<Child> {
    #[cfg(target_os = "linux")]
    let child = linux::spawn(cmd).await?;
    #[cfg(not(target_os = "linux"))]
    let child = {
        let mut cmd = cmd;
        cmd.spawn()?
    };

    #[cfg(windows)]
    windows::assign(&child);
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        pid_file::track(pid, command_line);
    }
    #[cfg(not(unix))]
    let _ = command_line;
    Ok(child)
}

/// Call when a process spawned by [`spawn`] has exited or been killed.
pub fn exited(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        pid_file::untrack(pid);
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// Call after killing every process on a normal quit.
pub fn all_stopped() {
    #[cfg(unix)]
    pid_file::clear();
}

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        io,
        sync::{mpsc, Mutex, OnceLock},
        thread,
    };

    use tokio::process::{Child, Command};

    type Job = Box<dyn FnOnce() + Send>;

    /// PR_SET_PDEATHSIG fires when the *thread* that spawned the child exits, not the
    /// process. Tokio's threads come and go, so every child is spawned from this one thread,
    /// which lives as long as the app.
    fn spawner() -> &'static Mutex<mpsc::Sender<Job>> {
        static SPAWNER: OnceLock<Mutex<mpsc::Sender<Job>>> = OnceLock::new();
        SPAWNER.get_or_init(|| {
            let (tx, rx) = mpsc::channel::<Job>();
            thread::Builder::new()
                .name("cloudflared-spawner".into())
                .spawn(move || {
                    for job in rx {
                        job();
                    }
                })
                .expect("failed to start the cloudflared spawner thread");
            Mutex::new(tx)
        })
    }

    pub async fn spawn(mut cmd: Command) -> io::Result<Child> {
        let parent = std::process::id() as libc::pid_t;
        // SAFETY: the closure only makes async-signal-safe syscalls (prctl, getppid).
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                    return Err(io::Error::last_os_error());
                }
                // Cloudfairy died between fork and prctl: the signal would never come.
                if libc::getppid() != parent {
                    return Err(io::Error::other("Cloudfairy exited while starting cloudflared"));
                }
                Ok(())
            });
        }

        // tokio needs its runtime to register the child for reaping.
        let runtime = tokio::runtime::Handle::current();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let job: Job = Box::new(move || {
            let _guard = runtime.enter();
            let _ = tx.send(cmd.spawn());
        });
        spawner()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(job)
            .map_err(|_| io::Error::other("the cloudflared spawner thread stopped"))?;
        rx.await
            .map_err(|_| io::Error::other("the cloudflared spawner thread stopped"))?
    }
}

#[cfg(windows)]
mod windows {
    use std::sync::OnceLock;

    use tokio::process::Child;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
    };

    /// The job's handle (stored as an integer because raw pointers aren't `Sync`); 0 if it
    /// couldn't be created. It's never closed: Windows closes it when Cloudfairy ends, and
    /// that kills every process in the job.
    fn job() -> Option<HANDLE> {
        static JOB: OnceLock<usize> = OnceLock::new();
        let handle = *JOB.get_or_init(|| unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return 0;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                CloseHandle(job);
                return 0;
            }
            job as usize
        });
        (handle != 0).then_some(handle as HANDLE)
    }

    pub fn assign(child: &Child) {
        if let (Some(job), Some(process)) = (job(), child.raw_handle()) {
            // If this fails the tunnel still works; it just isn't protected against a crash.
            unsafe { AssignProcessToJobObject(job, process as HANDLE) };
        }
    }
}

#[cfg(unix)]
mod pid_file {
    use std::{
        collections::BTreeMap,
        fs,
        path::PathBuf,
        process::Command,
        sync::{Mutex, MutexGuard},
    };

    struct State {
        path: PathBuf,
        /// PID -> the exact command line it was started with.
        running: BTreeMap<u32, String>,
    }

    static STATE: Mutex<Option<State>> = Mutex::new(None);

    fn state() -> MutexGuard<'static, Option<State>> {
        STATE.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn init(path: PathBuf) {
        if let Ok(text) = fs::read_to_string(&path) {
            for (pid, command_line) in parse(&text) {
                // Only kill a PID that is still running the same cloudflared command, so a
                // reused PID never takes down something else.
                if current_command_line(pid).as_deref() == Some(command_line.as_str()) {
                    // SAFETY: plain syscall; a stale PID just returns an error.
                    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
                }
            }
        }
        let _ = fs::remove_file(&path);
        *state() = Some(State {
            path,
            running: BTreeMap::new(),
        });
    }

    pub fn track(pid: u32, command_line: String) {
        if let Some(s) = state().as_mut() {
            s.running.insert(pid, command_line);
            save(s);
        }
    }

    pub fn untrack(pid: u32) {
        if let Some(s) = state().as_mut() {
            if s.running.remove(&pid).is_some() {
                save(s);
            }
        }
    }

    pub fn clear() {
        if let Some(s) = state().as_mut() {
            s.running.clear();
            save(s);
        }
    }

    fn save(s: &State) {
        if s.running.is_empty() {
            let _ = fs::remove_file(&s.path);
            return;
        }
        if let Some(dir) = s.path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let text: String = s
            .running
            .iter()
            .map(|(pid, cmd)| format!("{pid}\t{cmd}\n"))
            .collect();
        let _ = fs::write(&s.path, text);
    }

    pub(super) fn parse(text: &str) -> Vec<(u32, String)> {
        text.lines()
            .filter_map(|line| {
                let (pid, cmd) = line.split_once('\t')?;
                Some((pid.trim().parse().ok()?, cmd.to_string()))
            })
            .collect()
    }

    /// The process's program and arguments joined by spaces, as `ps` prints them.
    pub(super) fn current_command_line(pid: u32) -> Option<String> {
        let out = Command::new("ps")
            .args(["-ww", "-o", "command=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && !text.is_empty()).then_some(text)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_pid_file() {
            let text = "12\t/usr/bin/cloudflared tunnel --no-autoupdate --url http://localhost:3000\nbad line\n\n7\tx y\n";
            assert_eq!(
                parse(text),
                [
                    (12, "/usr/bin/cloudflared tunnel --no-autoupdate --url http://localhost:3000".to_string()),
                    (7, "x y".to_string()),
                ]
            );
        }

        #[test]
        fn reads_command_line_of_a_process() {
            let mut child = Command::new("sleep").arg("30").spawn().unwrap();
            let line = current_command_line(child.id());
            child.kill().unwrap();
            child.wait().unwrap();
            assert_eq!(line.as_deref(), Some("sleep 30"));
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use std::{
        io::{BufRead, BufReader},
        process::{Command as StdCommand, Stdio},
        time::{Duration, Instant},
    };

    const PARENT_ENV: &str = "CLOUDFAIRY_GUARD_TEST_PARENT";

    /// Re-runs this test binary as a stand-in for Cloudfairy, which spawns `sleep` through
    /// [`super::spawn`]. Then it SIGKILLs that parent, like a crash, and checks the child died too.
    #[test]
    fn child_dies_when_cloudfairy_is_killed() {
        if std::env::var_os(PARENT_ENV).is_some() {
            let pid = tauri::async_runtime::block_on(async {
                let mut cmd = tokio::process::Command::new("sleep");
                cmd.arg("300");
                let child = super::spawn(cmd, "sleep 300".into()).await.unwrap();
                let pid = child.id().unwrap();
                std::mem::forget(child); // don't let kill_on_drop do the work
                pid
            });
            println!("CHILD {pid}");
            std::thread::sleep(Duration::from_secs(300));
            return;
        }

        let mut parent = StdCommand::new(std::env::current_exe().unwrap())
            .args(["child_guard::linux_tests::child_dies_when_cloudfairy_is_killed", "--exact", "--nocapture"])
            .env(PARENT_ENV, "1")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let child_pid: i32 = BufReader::new(parent.stdout.take().unwrap())
            .lines()
            .map_while(Result::ok)
            .find_map(|l| l.strip_prefix("CHILD ").map(|p| p.trim().parse().unwrap()))
            .expect("parent printed the child's pid");
        assert!(alive(child_pid), "child is running before the parent dies");

        parent.kill().unwrap(); // SIGKILL: no exit handlers run
        parent.wait().unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        while alive(child_pid) {
            if Instant::now() > deadline {
                unsafe { libc::kill(child_pid, libc::SIGKILL) };
                panic!("child {child_pid} outlived its killed parent");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Running and not a zombie.
    fn alive(pid: i32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| s.rsplit_once(") ").map(|(_, rest)| !rest.starts_with('Z')))
            .unwrap_or(false)
    }
}
