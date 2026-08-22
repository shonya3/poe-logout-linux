use evdev::{Device, EventType, Key};
use std::{
    collections::HashSet,
    env, fs,
    io::{self},
    net::{IpAddr, Ipv4Addr},
    process::exit,
    thread,
    time::Duration,
};

/// Remote ports PoE game/realm servers listen on (6113 classic default,
/// 6112 also observed in the wild).
const GAME_PORTS: [u16; 2] = [6113, 6112];

/// The key that triggers logout: `~` (evdev `KEY_GRAVE`).
const HOTKEY: Key = Key::KEY_GRAVE;

fn main() {
    if env::args().nth(1).as_deref() == Some("--test") {
        return match logout() {
            Ok(n) => println!("done, destroyed {n}"),
            Err(e) => {
                eprintln!("error: {e}");
                exit(1)
            }
        };
    }
    if unsafe { libc::geteuid() } != 0 {
        // the daemon cannot work without root -> elevate ourselves once
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("cannot locate my own binary ({e}); run with sudo");
                exit(1);
            }
        };
        println!("requesting root (sudo)...");
        let status = std::process::Command::new("sudo")
            .arg(exe)
            .args(std::env::args_os().skip(1))
            .status();
        exit(status.ok().and_then(|s| s.code()).unwrap_or(1));
    }

    let mut paths = Vec::new();
    for entry in fs::read_dir("/dev/input")
        .expect("cannot read /dev/input")
        .flatten()
    {
        let p = entry.path();
        if !p
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("event"))
        {
            continue;
        }
        match Device::open(&p) {
            Ok(d) => {
                if d.supported_keys().is_some_and(|ks| ks.contains(HOTKEY)) {
                    println!("watching {} ({})", p.display(), d.name().unwrap_or("?"));
                    paths.push(p);
                }
            }
            Err(e) => eprintln!("skip {}: {e}", p.display()),
        }
    }
    if paths.is_empty() {
        eprintln!("no keyboard found supporting ~");
        exit(1);
    }

    match poe_pids().first() {
        Some(pid) => match live_game_peer() {
            Some((ip, port)) => {
                println!("Path of Exile: running (pid {pid}), session {ip}:{port}")
            }
            None => {
                println!("Path of Exile: running (pid {pid}) - no realm connection (login screen?)")
            }
        },
        None => println!("Path of Exile: not running"),
    }

    let mut handles = Vec::new();
    for path in paths {
        handles.push(thread::spawn(move || {
            let mut dev = Device::open(&path).expect("reopen");
            loop {
                match dev.fetch_events() {
                    Ok(events) => {
                        for ev in events {
                            if ev.event_type() == EventType::KEY
                                && ev.code() == HOTKEY.code()
                                && ev.value() == 1
                            {
                                println!("hotkey pressed -> logging out");
                                match logout() {
                                    Ok(n) => println!("destroyed {n} connection(s)"),
                                    Err(e) => eprintln!("logout failed: {e}"),
                                }
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("device error: {e}, retrying");
                        thread::sleep(Duration::from_secs(1));
                    }
                }
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
}

fn poe_pids() -> Vec<u32> {
    let mut out = Vec::new();
    for e in fs::read_dir("/proc").into_iter().flatten().flatten() {
        let pid: u32 = match e.file_name().to_string_lossy().parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        // Skip zombies: processes that already died but whose parent hasn't
        // collected the exit status yet. They keep a /proc entry (state 'Z')
        // but hold no fds/sockets, so counting them would fake "PoE running".
        // /proc/<pid>/stat looks like:  8225 (name) S 1 ...
        // rsplit_once(')') because the name itself may contain ')' (wine!);
        // whatever follows is the single-letter state.
        if let Ok(stat) = fs::read_to_string(e.path().join("stat"))
            && let Some(after) = stat.rsplit_once(')').map(|(_, s)| s.trim_start())
            && after.starts_with('Z')
        {
            continue;
        }
        if let Ok(cmd) = fs::read_to_string(e.path().join("cmdline")) {
            // the real game's argv[0] is the .exe itself (S:\...\PathOfExile*.exe);
            // launch wrappers (sh/reaper/python/bwrap) don't qualify
            let is_exe = cmd
                .split('\0')
                .next()
                .is_some_and(|a0| a0.to_lowercase().ends_with(".exe"));
            if is_exe && cmd.to_lowercase().contains("pathofexile") {
                out.push(pid);
            }
        }
    }
    out
}

/// Collect the kernel socket IDs owned by the given processes.
///
/// Every open fd of a process is a symlink in /proc/<pid>/fd; socket fds
/// point to names like `socket:[402153]`. The number inside matches the
/// `inode` column of /proc/net/tcp{,6}, letting us tell which connections
/// belong to PoE (the table itself has no PID info).
fn socket_inodes(pids: &[u32]) -> HashSet<u64> {
    let mut set = HashSet::new();
    for pid in pids {
        let dir = format!("/proc/{pid}/fd");
        for e in fs::read_dir(&dir).into_iter().flatten().flatten() {
            if let Ok(link) = fs::read_link(e.path()) {
                let s = link.to_string_lossy();
                if let Some(rest) = s.strip_prefix("socket:[")
                    && let Ok(ino) = rest.trim_end_matches(']').parse::<u64>()
                {
                    set.insert(ino);
                }
            }
        }
    }
    set
}

/// Decode an address column from `/proc/net/tcp` (`v6 == false`) or
/// `/proc/net/tcp6` (`v6 == true`), e.g. `"6800A8C0:17E3"` -> `(192.168.0.104, 6112)`.
///
/// The kernel dumps these tables in a hostile format:
/// * the whole entry is hexadecimal: `ADDRESS:PORT`
/// * the PORT part is a plain hex number (`17E3` == 6112, kernel applies ntohs)
/// * the ADDRESS part is raw in-memory bytes printed as hex, which on
///   little-endian machines appear **byte-swapped**:
///   - IPv4  `192.168.0.104` -> wire bytes `C0 A8 00 68` -> printed `6800A8C0`
///     (reverse all 4 bytes to recover it)
///   - IPv6  each 32-bit word is swapped on its own, word order preserved,
///     e.g. loopback `::1` prints as `00000000000000000000000001000000`
///
/// Returns `None` on malformed input.
fn hex_addr(h: &str, v6: bool) -> Option<(IpAddr, u16)> {
    let (ip, port) = h.rsplit_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    // /proc/net/tcp prints raw memory bytes %X -> reverse all byte-pairs
    if ip.len() != if v6 { 32 } else { 8 } {
        return None;
    }
    let mut b: Vec<u8> = (0..ip.len())
        .step_by(2)
        .filter_map(|i| u8::from_str_radix(&ip[i..i + 2], 16).ok())
        .collect();
    if b.len() != ip.len() / 2 {
        return None;
    }
    let ip = if !v6 {
        IpAddr::V4(Ipv4Addr::new(b[3], b[2], b[1], b[0]))
    } else {
        // each 32-bit word of tcp6 is printed byte-swapped; words keep order
        for w in 0..4 {
            b[w * 4..w * 4 + 4].reverse();
        }
        let arr: [u8; 16] = b.as_slice().try_into().ok()?;
        IpAddr::V6(arr.into())
    };
    Some((ip, port))
}

/// one established TCP connection learned from /proc/net/tcp{,6}
struct Conn {
    inode: u64,
    local: (IpAddr, u16),
    peer: (IpAddr, u16),
}

fn tcp_established() -> Vec<Conn> {
    let mut out = Vec::new();
    for (file, v6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 10 || f[3] != "01" {
                continue;
            }
            let Ok(ino) = f[9].parse::<u64>() else {
                continue;
            };
            if let (Some(l), Some(p)) = (hex_addr(f[1], v6), hex_addr(f[2], v6)) {
                out.push(Conn {
                    inode: ino,
                    local: l,
                    peer: p,
                });
            }
        }
    }
    out
}

/// Count current ESTABLISHED connections whose *remote* (peer) port is `port`.
///
/// Used to check a `ss -K` kill actually worked: count before, kill,
/// count again - if still above zero, the connection survived.
fn established_to_port(port: u16) -> usize {
    tcp_established()
        .iter()
        .filter(|c| c.peer.1 == port)
        .count()
}

enum KillStatus {
    NothingConnected,
    Destroyed,
    StillAlive,
}

fn kill_port(port: u16) -> KillStatus {
    let before = established_to_port(port);
    if before == 0 {
        return KillStatus::NothingConnected;
    }
    let out = match std::process::Command::new("ss")
        .args(["-K", &format!("dst :{port}")])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("cannot run ss: {e}");
            return KillStatus::StillAlive;
        }
    };
    let after = established_to_port(port);
    if after == 0 {
        println!("port {port}: connection destroyed");
        return KillStatus::Destroyed;
    }
    // genuine failure -> now show ss's complaint too
    let errtxt = String::from_utf8_lossy(&out.stderr);
    if !errtxt.trim().is_empty() {
        eprintln!("{}", errtxt.trim());
    }
    eprintln!("port {port}: STILL ALIVE ({after} connections) - are you root?");
    KillStatus::StillAlive
}

// live realm/game session = any established socket to the known game ports
/// Server address of a live PoE session, if any: the first ESTABLISHED
/// connection whose remote port is one of [`GAME_PORTS`].
///
/// Port-based and display-only (startup status line). The actual kill path
/// in `logout()` is stricter: it matches sockets by owning process.
fn live_game_peer() -> Option<(IpAddr, u16)> {
    tcp_established()
        .into_iter()
        .find(|c| GAME_PORTS.contains(&c.peer.1))
        .map(|c| c.peer)
}

fn logout() -> io::Result<usize> {
    let pids = poe_pids();
    if pids.is_empty() {
        eprintln!("Path of Exile not running");
        return Ok(0);
    }
    let inodes = socket_inodes(&pids);
    let socks: Vec<_> = tcp_established()
        .into_iter()
        .filter(|c| inodes.contains(&c.inode))
        .map(|c| (c.local, c.peer))
        .collect();

    // unique remote ports of the game's real (non-loopback) connections
    let mut ports: Vec<u16> = Vec::new();
    for ((_, _), (rip, rport)) in &socks {
        let loopback = matches!(rip, IpAddr::V4(a) if a.is_loopback())
            || matches!(rip, IpAddr::V6(a) if a.is_loopback());
        if !loopback && *rport >= 1024 && !ports.contains(rport) {
            ports.push(*rport);
        }
    }
    if ports.is_empty() {
        // fallback sweep: nothing found via process ownership,
        // so target every port in GAME_PORTS blindly
        ports.extend_from_slice(&GAME_PORTS);
    }

    let mut n = 0;
    let mut any_target = false;
    for p in &ports {
        match kill_port(*p) {
            KillStatus::Destroyed => n += 1,
            KillStatus::StillAlive => any_target = true,
            KillStatus::NothingConnected => {}
        }
    }
    if !any_target && n == 0 {
        println!("no live game connection (login screen or already out)");
    }
    Ok(n)
}
