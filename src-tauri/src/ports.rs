//! Finds local TCP ports that something listens on, to suggest them when adding a tunnel.
//!
//! The OS listing comes from `GetExtendedTcpTable` on Windows, `/proc/net/tcp*` on Linux and
//! `lsof` on macOS. Where that fails, Cloudfairy tries to connect to common dev server ports.

use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};

use serde::Serialize;

/// How many suggestions the window shows at most.
const MAX_SUGGESTIONS: usize = 8;
/// The whole lookup gives up after this long, so the window never waits on it.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);
const PROBE_TIMEOUT: Duration = Duration::from_millis(150);

/// Ports dev servers use by default. Probed when the OS listing isn't available, and listed
/// first when it is.
const COMMON_PORTS: &[u16] = &[
    3000, 3001, 3002, 3003, 3004, 3005, 3006, 3007, 3008, 3009, 3010, 4000, 4200, 4321, 5000,
    5001, 5173, 5174, 5500, 6006, 8000, 8001, 8008, 8080, 8081, 8443, 8787, 8888, 9000, 9090,
];

/// Programs that usually run a dev server, listed before other programs.
const DEV_PROGRAMS: &[&str] = &[
    "node", "deno", "bun", "python", "python3", "pythonw", "py", "ruby", "php", "java", "dotnet",
    "iisexpress", "uvicorn", "gunicorn", "hugo", "caddy", "nginx", "httpd", "apache2", "docker",
    "docker-proxy", "com.docker.backend", "wslrelay", "beam.smp", "esbuild", "air",
];

/// OS services that listen on ports nobody tunnels to.
const SYSTEM_PROGRAMS: &[&str] = &[
    "system", "svchost", "lsass", "wininit", "services", "spoolsv", "searchindexer", "systemd",
    "systemd-resolved", "systemd-resolve", "sshd", "cupsd", "rpcbind", "avahi-daemon",
    "dnsmasq", "launchd", "rapportd", "sharingd", "controlce", "controlcenter", "mdnsresponder",
    "remoted", "airplayxpchelper",
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Suggestion {
    pub port: u16,
    /// The program listening on the port, when the OS says.
    pub process: Option<String>,
    /// What goes in the address field when the suggestion is clicked.
    pub value: String,
}

/// One listening socket from the OS.
#[derive(Clone, Debug)]
struct Listener {
    addr: IpAddr,
    port: u16,
    pid: Option<u32>,
    process: Option<String>,
}

/// Lists suggestions, leaving out the given processes and ports. Never takes longer than
/// [`LOOKUP_TIMEOUT`]; on timeout it returns nothing.
pub async fn suggest(exclude_pids: Vec<u32>, exclude_ports: Vec<u16>) -> Vec<Suggestion> {
    let lookup = async {
        let listeners = match os_listeners().await {
            Some(list) => list,
            None => probe_common(&exclude_ports).await,
        };
        let mut pids: HashSet<u32> = exclude_pids.into_iter().collect();
        pids.insert(std::process::id());
        pick(listeners, &pids, &exclude_ports.into_iter().collect())
    };
    tokio::time::timeout(LOOKUP_TIMEOUT, lookup).await.unwrap_or_default()
}

/// Turns raw listeners into suggestions: only addresses `localhost` reaches, no excluded
/// owners or ports, no OS services, one entry per port, likely dev servers first.
fn pick(listeners: Vec<Listener>, exclude_pids: &HashSet<u32>, exclude_ports: &HashSet<u16>) -> Vec<Suggestion> {
    let mut by_port: HashMap<u16, Option<String>> = HashMap::new();
    for l in listeners {
        let reachable = l.addr.is_loopback() || l.addr.is_unspecified();
        let owned = l.pid.is_some_and(|p| exclude_pids.contains(&p) || p == 0 || (cfg!(windows) && p == 4));
        let name = l.process.as_deref().map(program_name);
        let system = name.as_deref().is_some_and(|n| SYSTEM_PROGRAMS.contains(&n));
        let dev = name.as_deref().is_some_and(|n| DEV_PROGRAMS.contains(&n));
        let low = l.port < 1024 && !matches!(l.port, 80 | 443);
        // Many apps listen on high ports for their own use; only dev tools are kept there.
        let ephemeral = l.port >= 32768 && !dev;
        if !reachable || owned || system || low || ephemeral || exclude_ports.contains(&l.port) {
            continue;
        }
        let entry = by_port.entry(l.port).or_default();
        if entry.is_none() {
            *entry = l.process.map(|p| display_name(&p));
        }
    }
    let mut list: Vec<Suggestion> = by_port
        .into_iter()
        .map(|(port, process)| Suggestion {
            port,
            value: match port {
                443 | 8443 => format!("https://localhost:{port}"),
                _ => port.to_string(),
            },
            process,
        })
        .collect();
    list.sort_by_key(|s| {
        let dev = s.process.as_deref().is_some_and(|p| DEV_PROGRAMS.contains(&program_name(p).as_str()));
        (!dev && !COMMON_PORTS.contains(&s.port), s.port)
    });
    list.truncate(MAX_SUGGESTIONS);
    list
}

/// `C:\Program Files\nodejs\node.exe` becomes `node`.
fn display_name(process: &str) -> String {
    let name = process.rsplit(['/', '\\']).next().unwrap_or(process);
    match name.len().checked_sub(4) {
        Some(i) if name.is_char_boundary(i) && name[i..].eq_ignore_ascii_case(".exe") => name[..i].to_string(),
        _ => name.to_string(),
    }
}

/// Lowercase name without folder or `.exe`, for matching against the lists above.
fn program_name(process: &str) -> String {
    display_name(process).to_ascii_lowercase()
}

/// Tries each common port on IPv4 and IPv6 loopback at once.
async fn probe_common(skip: &[u16]) -> Vec<Listener> {
    let mut tasks = Vec::new();
    for &port in COMMON_PORTS.iter().filter(|p| !skip.contains(p)) {
        for ip in [IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V6(Ipv6Addr::LOCALHOST)] {
            tasks.push(tauri::async_runtime::spawn(async move {
                let connect = tokio::net::TcpStream::connect(SocketAddr::new(ip, port));
                matches!(tokio::time::timeout(PROBE_TIMEOUT, connect).await, Ok(Ok(_))).then_some(Listener {
                    addr: ip,
                    port,
                    pid: None,
                    process: None,
                })
            }));
        }
    }
    let mut found = Vec::new();
    for task in tasks {
        if let Ok(Some(l)) = task.await {
            found.push(l);
        }
    }
    found
}

#[cfg(windows)]
async fn os_listeners() -> Option<Vec<Listener>> {
    tauri::async_runtime::spawn_blocking(windows::listeners).await.ok().flatten()
}

#[cfg(target_os = "linux")]
async fn os_listeners() -> Option<Vec<Listener>> {
    tauri::async_runtime::spawn_blocking(linux::listeners).await.ok().flatten()
}

#[cfg(target_os = "macos")]
async fn os_listeners() -> Option<Vec<Listener>> {
    let mut cmd = tokio::process::Command::new("lsof");
    cmd.args(["-nP", "-iTCP", "-sTCP:LISTEN", "-Fpcn"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(2), cmd.output()).await.ok()?.ok()?;
    // lsof exits with 1 when nothing matches, which is still a valid (empty) answer.
    let text = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() && !text.is_empty() {
        return None;
    }
    Some(parse_lsof(&text))
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
async fn os_listeners() -> Option<Vec<Listener>> {
    None
}

/// Parses `lsof -F pcn` output: `p<pid>`, `c<command>`, then `n<address>:<port>` per socket.
#[cfg(any(target_os = "macos", test))]
fn parse_lsof(text: &str) -> Vec<Listener> {
    let (mut pid, mut process) = (None, None);
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(tag) = line.chars().next() else { continue };
        let value = &line[tag.len_utf8()..];
        match tag {
            'p' => {
                pid = value.parse().ok();
                process = None;
            }
            'c' => process = Some(value.to_string()),
            'n' => {
                let Some((host, port)) = value.rsplit_once(':') else { continue };
                let Ok(port) = port.parse() else { continue };
                let addr = match host {
                    "*" => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    h => match h.trim_matches(['[', ']']).parse() {
                        Ok(ip) => ip,
                        Err(_) => continue,
                    },
                };
                out.push(Listener { addr, port, pid, process: process.clone() });
            }
            _ => {}
        }
    }
    out
}

/// Parses `/proc/net/tcp` or `/proc/net/tcp6`. Returns listening sockets with their inode.
#[cfg(any(target_os = "linux", test))]
fn parse_proc_net(text: &str) -> Vec<(IpAddr, u16, u64)> {
    const LISTEN: &str = "0A";
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 10 || fields[3] != LISTEN {
            continue;
        }
        let Some((ip_hex, port_hex)) = fields[1].split_once(':') else { continue };
        let Ok(port) = u16::from_str_radix(port_hex, 16) else { continue };
        let Ok(inode) = fields[9].parse() else { continue };
        // The kernel prints each 32-bit word of the address in host byte order.
        let mut bytes = Vec::with_capacity(16);
        for chunk in ip_hex.as_bytes().chunks(8) {
            let Some(word) = std::str::from_utf8(chunk).ok().and_then(|c| u32::from_str_radix(c, 16).ok()) else {
                break;
            };
            bytes.extend_from_slice(&word.to_ne_bytes());
        }
        let addr = match bytes.len() {
            4 => IpAddr::V4(Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3])),
            16 => {
                let mut b = [0u8; 16];
                b.copy_from_slice(&bytes);
                IpAddr::V6(Ipv6Addr::from(b))
            }
            _ => continue,
        };
        out.push((addr, port, inode));
    }
    out
}

#[cfg(target_os = "linux")]
mod linux {
    use std::{collections::HashMap, fs};

    use super::{parse_proc_net, Listener};

    pub fn listeners() -> Option<Vec<Listener>> {
        let mut sockets = Vec::new();
        let mut any = false;
        for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
            if let Ok(text) = fs::read_to_string(file) {
                any = true;
                sockets.extend(parse_proc_net(&text));
            }
        }
        if !any {
            return None;
        }
        let owners = owners(sockets.iter().map(|s| s.2).collect());
        Some(
            sockets
                .into_iter()
                .map(|(addr, port, inode)| {
                    let owner = owners.get(&inode);
                    Listener {
                        addr,
                        port,
                        pid: owner.map(|o| o.0),
                        process: owner.map(|o| o.1.clone()),
                    }
                })
                .collect(),
        )
    }

    /// Maps socket inodes to the process that has them open. Only the user's own processes
    /// are readable, which covers dev servers.
    fn owners(mut wanted: std::collections::HashSet<u64>) -> HashMap<u64, (u32, String)> {
        let mut found = HashMap::new();
        let Ok(procs) = fs::read_dir("/proc") else { return found };
        for entry in procs.flatten() {
            if wanted.is_empty() {
                break;
            }
            let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
            let Ok(fds) = fs::read_dir(entry.path().join("fd")) else { continue };
            for fd in fds.flatten() {
                let Ok(link) = fs::read_link(fd.path()) else { continue };
                let Some(inode) = link
                    .to_str()
                    .and_then(|l| l.strip_prefix("socket:["))
                    .and_then(|l| l.strip_suffix(']'))
                    .and_then(|l| l.parse::<u64>().ok())
                else {
                    continue;
                };
                if wanted.remove(&inode) {
                    let name = fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
                    found.insert(inode, (pid, name.trim().to_string()));
                }
            }
        }
        found
    }
}

#[cfg(windows)]
mod windows {
    use std::{
        collections::HashMap,
        net::{IpAddr, Ipv4Addr, Ipv6Addr},
    };

    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, INVALID_HANDLE_VALUE, NO_ERROR},
        NetworkManagement::IpHelper::{
            GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCP6TABLE_OWNER_PID, MIB_TCPROW_OWNER_PID,
            MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
        },
        Networking::WinSock::{AF_INET, AF_INET6},
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
        },
    };

    use super::Listener;

    pub fn listeners() -> Option<Vec<Listener>> {
        let mut out = Vec::new();
        let v4 = table(AF_INET.into());
        let v6 = table(AF_INET6.into());
        if v4.is_none() && v6.is_none() {
            return None;
        }
        if let Some(buf) = v4 {
            // SAFETY: `table` returned a buffer the API filled with a MIB_TCPTABLE_OWNER_PID,
            // and `rows` checks the entry count against the buffer length.
            for row in unsafe { rows::<MIB_TCPTABLE_OWNER_PID, MIB_TCPROW_OWNER_PID>(&buf) } {
                out.push(Listener {
                    addr: IpAddr::V4(Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes())),
                    port: u16::from_be(row.dwLocalPort as u16),
                    pid: Some(row.dwOwningPid),
                    process: None,
                });
            }
        }
        if let Some(buf) = v6 {
            // SAFETY: as above, for MIB_TCP6TABLE_OWNER_PID.
            for row in unsafe { rows::<MIB_TCP6TABLE_OWNER_PID, MIB_TCP6ROW_OWNER_PID>(&buf) } {
                out.push(Listener {
                    addr: IpAddr::V6(Ipv6Addr::from(row.ucLocalAddr)),
                    port: u16::from_be(row.dwLocalPort as u16),
                    pid: Some(row.dwOwningPid),
                    process: None,
                });
            }
        }
        let names = process_names();
        for l in &mut out {
            l.process = l.pid.and_then(|p| names.get(&p).cloned());
        }
        Some(out)
    }

    /// Reads the listening-socket table for one address family. The buffer is made of u64s
    /// so the table is suitably aligned.
    fn table(family: u32) -> Option<Vec<u64>> {
        let mut size: u32 = 0;
        // SAFETY: a null buffer with size 0 asks for the size needed.
        unsafe { GetExtendedTcpTable(std::ptr::null_mut(), &mut size, 0, family, TCP_TABLE_OWNER_PID_LISTENER, 0) };
        for _ in 0..4 {
            // Leave room for sockets opened between the two calls.
            let mut buf = vec![0u64; (size as usize).div_ceil(8) + 64];
            size = (buf.len() * 8) as u32;
            // SAFETY: `buf` is writable for `size` bytes.
            let result = unsafe {
                GetExtendedTcpTable(buf.as_mut_ptr().cast(), &mut size, 0, family, TCP_TABLE_OWNER_PID_LISTENER, 0)
            };
            match result {
                NO_ERROR => return Some(buf),
                ERROR_INSUFFICIENT_BUFFER => continue,
                _ => return None,
            }
        }
        None
    }

    /// The rows of a `{ dwNumEntries: u32, table: [Row; 1] }` table.
    ///
    /// # Safety
    /// `buf` must hold a table of type `T` written by `GetExtendedTcpTable`.
    unsafe fn rows<T, Row: Copy>(buf: &[u64]) -> Vec<Row> {
        let bytes = buf.len() * 8;
        let count = *(buf.as_ptr() as *const u32) as usize;
        // The rows start after the count, padded to the row alignment.
        let offset = std::mem::size_of::<T>() - std::mem::size_of::<Row>();
        if offset + count.saturating_mul(std::mem::size_of::<Row>()) > bytes {
            return Vec::new();
        }
        let first = (buf.as_ptr() as *const u8).add(offset) as *const Row;
        std::slice::from_raw_parts(first, count).to_vec()
    }

    /// PID to executable name for every process, from one snapshot.
    fn process_names() -> HashMap<u32, String> {
        let mut names = HashMap::new();
        // SAFETY: plain Win32 calls; the handle is closed below and the entry's size is set.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap.is_null() || snap == INVALID_HANDLE_VALUE {
                return names;
            }
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut ok = Process32FirstW(snap, &mut entry);
            while ok != 0 {
                let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
                names.insert(entry.th32ProcessID, String::from_utf16_lossy(&entry.szExeFile[..len]));
                ok = Process32NextW(snap, &mut entry);
            }
            CloseHandle(snap);
        }
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(addr: &str, port: u16, pid: u32, process: &str) -> Listener {
        Listener {
            addr: addr.parse().unwrap(),
            port,
            pid: Some(pid),
            process: (!process.is_empty()).then(|| process.to_string()),
        }
    }

    #[test]
    fn picks_dev_servers_and_skips_the_rest() {
        let listeners = vec![
            l("127.0.0.1", 5173, 10, "node.exe"),
            l("::1", 5173, 10, "node.exe"),
            l("0.0.0.0", 135, 900, "svchost.exe"),
            l("0.0.0.0", 7000, 11, "ControlCe"),
            l("192.168.1.5", 3000, 12, "node"),
            l("127.0.0.1", 8080, 13, ""),
            l("127.0.0.1", 9100, 14, "myapp"),
            l("127.0.0.1", 51234, 15, "Discord.exe"),
            l("127.0.0.1", 51235, 16, "python3"),
            l("127.0.0.1", 41000, 77, "cloudflared"),
            l("127.0.0.1", 3001, 20, "node"),
            l("127.0.0.1", 8443, 21, "caddy"),
        ];
        let pids = HashSet::from([77]);
        let ports = HashSet::from([3001]);
        let got: Vec<(u16, Option<String>, String)> =
            pick(listeners, &pids, &ports).into_iter().map(|s| (s.port, s.process, s.value)).collect();
        assert_eq!(
            got,
            vec![
                (5173, Some("node".into()), "5173".into()),
                (8080, None, "8080".into()),
                (8443, Some("caddy".into()), "https://localhost:8443".into()),
                (51235, Some("python3".into()), "51235".into()),
                (9100, Some("myapp".into()), "9100".into()),
            ]
        );
    }

    #[test]
    fn keeps_at_most_a_few() {
        let listeners = (0..20).map(|i| l("127.0.0.1", 3000 + i, 1000, "node")).collect();
        assert_eq!(pick(listeners, &HashSet::new(), &HashSet::new()).len(), MAX_SUGGESTIONS);
    }

    #[test]
    fn reads_lsof_output() {
        let text = "p501\ncnode\nf23\nn*:3000\nf24\nn[::1]:5173\np88\ncControlCe\nf9\nn127.0.0.1:7000\n";
        let got: Vec<(String, u16, Option<u32>, Option<String>)> = parse_lsof(text)
            .into_iter()
            .map(|l| (l.addr.to_string(), l.port, l.pid, l.process))
            .collect();
        assert_eq!(
            got,
            vec![
                ("0.0.0.0".into(), 3000, Some(501), Some("node".into())),
                ("::1".into(), 5173, Some(501), Some("node".into())),
                ("127.0.0.1".into(), 7000, Some(88), Some("ControlCe".into())),
            ]
        );
    }

    #[test]
    fn reads_proc_net_tcp() {
        // 127.0.0.1:8080 listening, and an established connection that is skipped.
        let local = u32::from_ne_bytes([127, 0, 0, 1]);
        let text = format!(
            "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
             \x20  0: {local:08X}:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4242 1 0 100 0 0 10 0\n\
             \x20  1: {local:08X}:1F90 {local:08X}:C000 01 00000000:00000000 00:00000000 00000000  1000        0 4243 1 0 100 0 0 10 0\n"
        );
        assert_eq!(parse_proc_net(&text), vec![("127.0.0.1".parse().unwrap(), 8080, 4242)]);

        let one = u32::from_ne_bytes([0, 0, 0, 1]);
        let v6 = format!(
            "header\n   0: 000000000000000000000000{one:08X}:14E5 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 99 1\n"
        );
        assert_eq!(parse_proc_net(&v6), vec![("::1".parse().unwrap(), 5349, 99)]);
    }

    #[test]
    fn os_listing_sees_own_socket_and_suggestions_skip_it() {
        tauri::async_runtime::block_on(async {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            if let Some(list) = os_listeners().await {
                let mine = list.iter().find(|l| l.port == port).expect("own listener is listed");
                assert_eq!(mine.pid, Some(std::process::id()));
                assert!(mine.process.is_some(), "process name is read");
            }
            assert!(suggest(Vec::new(), Vec::new()).await.iter().all(|s| s.port != port));
            drop(listener);
        });
    }

    /// Shows what this computer would suggest:
    /// `cargo test -- --ignored --nocapture lists_this_computer`
    #[test]
    #[ignore = "prints this computer's listening ports"]
    fn lists_this_computer() {
        let started = std::time::Instant::now();
        let found = tauri::async_runtime::block_on(suggest(Vec::new(), Vec::new()));
        println!("{found:#?}\nin {:?}", started.elapsed());
    }

    #[test]
    fn names_programs() {
        assert_eq!(display_name(r"C:\Program Files\nodejs\node.exe"), "node");
        assert_eq!(display_name("/usr/bin/python3"), "python3");
        assert_eq!(program_name("Docker.EXE"), "docker");
    }
}
