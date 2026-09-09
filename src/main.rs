//! herdr-sleep: free RAM from idle Claude Code chats in herdr panes without losing them.
//!
//! An idle chat is a Node process holding 300-1000 MB. The conversation is already on disk
//! in `~/.claude/projects/<cwd-slug>/<id>.jsonl`, so exiting loses nothing and
//! `claude --resume <id>` brings it back.
//!
//! sleep = send `/exit`, then park the pane's shell on a "press Enter to wake" wait.
//! wake  = press Enter in that pane (by hand or from this CLI).

use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_HOURS: f64 = 24.0;
const PARK_MARK: &str = "press Enter to wake this chat";
/// The park waits in an external process so the pane process list shows the state.
/// Screen text is unreliable: a chat leaving its alt screen restores stale text.
const PARK_PROC: &str = "head -n1";
const LAUNCHD_LABEL: &str = "dev.herdr-sleep";

// ---------- CLI ----------

#[derive(Parser)]
#[command(name = "herdr-sleep", version, about = "Free RAM from idle Claude Code chats in herdr. Sleep them, wake them with Enter.")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Every chat: idle time, RAM, and who would sleep at the cutoff
    List {
        /// Idle cutoff in hours used for the "*" marker
        #[arg(long, default_value_t = DEFAULT_HOURS)]
        hours: f64,
    },
    /// Exit chats and park their panes. Pass panes, or --all-idle
    Sleep {
        /// herdr session name
        session: Option<String>,
        /// Pane ids in that session, e.g. w1:p3
        panes: Vec<String>,
        /// Sleep every idle or done chat older than --hours
        #[arg(long)]
        all_idle: bool,
        #[arg(long, default_value_t = DEFAULT_HOURS)]
        hours: f64,
        #[arg(long)]
        dry_run: bool,
    },
    /// Resume sleeping chats in their panes. Pass panes, or --all
    Wake {
        session: Option<String>,
        panes: Vec<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        dry_run: bool,
    },
    /// One auto-sleep pass: same as `sleep --all-idle`. What the timer runs
    Auto {
        #[arg(long, default_value_t = DEFAULT_HOURS)]
        hours: f64,
        #[arg(long)]
        dry_run: bool,
    },
    /// Install a launchd timer (macOS) that runs `auto`
    Install {
        #[arg(long, default_value_t = DEFAULT_HOURS)]
        hours: f64,
        /// Interval like 15m or 1h
        #[arg(long, default_value = "15m")]
        every: String,
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove the launchd timer
    Uninstall,
}

// ---------- paths ----------

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME is not set"))
}
fn state_dir() -> PathBuf {
    home().join(".config").join("herdr-sleep")
}
fn state_file() -> PathBuf {
    state_dir().join("sleeping.json")
}
fn log_file() -> PathBuf {
    state_dir().join("auto.log")
}
fn claude_projects() -> PathBuf {
    match std::env::var("CLAUDE_CONFIG_DIR") {
        Ok(d) => PathBuf::from(d).join("projects"),
        Err(_) => home().join(".claude").join("projects"),
    }
}

// ---------- herdr ----------

fn herdr(session: Option<&str>, args: &[&str]) -> Result<Value, String> {
    let mut cmd = Command::new("herdr");
    if let Some(s) = session {
        cmd.arg("--session").arg(s);
    }
    cmd.args(args);
    let out = cmd.output().map_err(|e| format!("herdr not found: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if stdout.starts_with('{') {
        let v: Value = serde_json::from_str(&stdout).map_err(|e| format!("bad json from herdr: {e}"))?;
        if let Some(err) = v.get("error") {
            let msg = err.get("message").and_then(Value::as_str).unwrap_or("error");
            return Err(format!("herdr {}: {msg}", args.join(" ")));
        }
        return Ok(v.get("result").cloned().unwrap_or(v));
    }
    if !out.status.success() {
        return Err(format!("herdr {}: {}", args.join(" "), if stderr.is_empty() { stdout } else { stderr }));
    }
    Ok(Value::String(stdout))
}

/// herdr call scoped to one session.
fn hs(session: &str, args: &[&str]) -> Result<Value, String> {
    herdr(Some(session), args)
}

fn sessions() -> Vec<String> {
    let v = herdr(None, &["session", "list", "--json"]).unwrap_or(Value::Null);
    v.get("sessions")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter(|s| s.get("running").and_then(Value::as_bool).unwrap_or(false))
                .filter_map(|s| s.get("name").and_then(Value::as_str).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

// ---------- state ----------

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Sleeping {
    session: String,
    pane: String,
    tab: String,
    name: String,
    sid: String,
    cwd: String,
    /// Launch flags the chat had, minus --resume. Reused on wake.
    #[serde(default)]
    args: Vec<String>,
    #[serde(alias = "sleptAt")]
    slept_at: String,
    #[serde(alias = "idleHours")]
    idle_hours: f64,
}

type State = BTreeMap<String, Sleeping>;

fn load_state() -> State {
    fs::read_to_string(state_file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}
fn save_state(s: &State) {
    let _ = fs::create_dir_all(state_dir());
    let _ = fs::write(state_file(), serde_json::to_string_pretty(s).unwrap() + "\n");
}
fn key(session: &str, pane: &str) -> String {
    format!("{session}/{pane}")
}

fn now_iso() -> String {
    iso8601(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs())
}

fn log(line: &str) {
    let msg = format!("{} {line}", now_iso());
    println!("{msg}");
    let _ = fs::create_dir_all(state_dir());
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(log_file()) {
        use std::io::Write;
        let _ = writeln!(f, "{msg}");
    }
}

// ---------- time helpers (no chrono: only UTC, only ISO-8601) ----------

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = (m as u64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i64 - 719468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// "2026-09-08T16:12:31.261Z" -> unix seconds. Returns None on anything else.
fn parse_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |a: usize, e: usize| s.get(a..e)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)? as u32, num(8, 10)? as u32);
    let (h, mi, se) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    Some(days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + se)
}

fn iso8601(secs: u64) -> String {
    let (y, m, d) = civil_from_days((secs / 86400) as i64);
    let r = secs % 86400;
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", r / 3600, (r % 3600) / 60, r % 60)
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

// ---------- transcript idle time ----------

fn slug(cwd: &str) -> String {
    cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

fn transcript_path(cwd: &str, sid: &str) -> Option<PathBuf> {
    let direct = claude_projects().join(slug(cwd)).join(format!("{sid}.jsonl"));
    if direct.exists() {
        return Some(direct);
    }
    // The cwd may have changed after launch. Look in every project folder.
    for entry in fs::read_dir(claude_projects()).ok()?.flatten() {
        let p = entry.path().join(format!("{sid}.jsonl"));
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Time of the last user or assistant row, read from the file tail.
/// File mtime lies: Remote Control appends "bridge-session" rows on reconnect.
fn last_message_secs(path: &Path) -> Option<i64> {
    let mut f = fs::File::open(path).ok()?;
    let size = f.metadata().ok()?.len();
    let chunk: u64 = 512 * 1024;
    let mut end = size;
    while end > 0 {
        let start = end.saturating_sub(chunk);
        f.seek(SeekFrom::Start(start)).ok()?;
        let mut buf = vec![0u8; (end - start) as usize];
        f.read_exact(&mut buf).ok()?;
        let text = String::from_utf8_lossy(&buf);
        for line in text.lines().rev() {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                let t = v.get("type").and_then(Value::as_str).unwrap_or("");
                if t == "user" || t == "assistant" {
                    if let Some(ts) = v.get("timestamp").and_then(Value::as_str).and_then(parse_iso) {
                        return Some(ts);
                    }
                }
            }
        }
        end = start;
    }
    None
}

fn idle_hours(cwd: &str, sid: &str) -> Option<f64> {
    let p = transcript_path(cwd, sid)?;
    let last = last_message_secs(&p)?;
    Some((now_secs() - last) as f64 / 3600.0)
}

// ---------- processes ----------

/// RSS in KB summed per process group. A chat is claude plus its MCP children.
fn rss_by_pgid() -> BTreeMap<u32, u64> {
    let out = Command::new("ps").args(["-axo", "rss,pgid"]).output();
    let mut m = BTreeMap::new();
    if let Ok(o) = out {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let mut it = line.split_whitespace();
            if let (Some(rss), Some(pg)) = (it.next(), it.next()) {
                if let (Ok(rss), Ok(pg)) = (rss.parse::<u64>(), pg.parse::<u32>()) {
                    *m.entry(pg).or_insert(0) += rss;
                }
            }
        }
    }
    m
}

struct PaneProcs {
    pgid: Option<u32>,
    fg: Vec<String>,
    claude_argv: Option<Vec<String>>,
}

fn pane_procs(session: &str, pane: &str) -> PaneProcs {
    let v = hs(session, &["pane", "process-info", "--pane", pane]).unwrap_or(Value::Null);
    let info = v.get("process_info").cloned().unwrap_or(Value::Null);
    let pgid = info.get("foreground_process_group_id").and_then(Value::as_u64).map(|x| x as u32);
    let mut fg = Vec::new();
    let mut claude_argv = None;
    if let Some(list) = info.get("foreground_processes").and_then(Value::as_array) {
        for p in list {
            let cmd = p.get("cmdline").and_then(Value::as_str).unwrap_or("").trim().to_string();
            let argv: Vec<String> = p
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
                .unwrap_or_default();
            if claude_argv.is_none() && argv.first().map(|a| a.ends_with("claude")).unwrap_or(false) {
                claude_argv = Some(argv.clone());
            }
            fg.push(cmd);
        }
    }
    PaneProcs { pgid, fg, claude_argv }
}

fn is_shell(c: &str) -> bool {
    let c = c.trim_start_matches('-');
    matches!(c, "zsh" | "bash" | "fish" | "sh") || c.rsplit('/').next().map(|b| matches!(b, "zsh" | "bash" | "fish" | "sh")).unwrap_or(false)
}
fn is_parked(session: &str, pane: &str) -> bool {
    pane_procs(session, pane).fg.iter().any(|c| c.starts_with(PARK_PROC))
}
fn at_prompt(session: &str, pane: &str) -> bool {
    let fg = pane_procs(session, pane).fg;
    !fg.is_empty() && fg.iter().all(|c| is_shell(c))
}
fn wait_for_prompt(session: &str, pane: &str, secs: u64) -> bool {
    for _ in 0..secs * 2 {
        if at_prompt(session, pane) {
            return true;
        }
        sleep(Duration::from_millis(500));
    }
    at_prompt(session, pane)
}
fn pane_has_agent(session: &str, pane: &str) -> bool {
    hs(session, &["agent", "list"])
        .ok()
        .and_then(|v| v.get("agents").and_then(Value::as_array).cloned())
        .map(|a| a.iter().any(|x| x.get("pane_id").and_then(Value::as_str) == Some(pane)))
        .unwrap_or(false)
}
fn wait_for_agent(session: &str, pane: &str, secs: u64) -> bool {
    for _ in 0..secs * 2 {
        if pane_has_agent(session, pane) {
            return true;
        }
        sleep(Duration::from_millis(500));
    }
    pane_has_agent(session, pane)
}

/// Keep the flags a chat was launched with, drop the ones that pick a conversation
/// and any positional prompt. These are reused on wake, after `--resume <id>`.
fn launch_flags(argv: &[String]) -> Vec<String> {
    const DROP_WITH_VALUE: &[&str] = &["--resume", "-r", "--session-id", "--fork-session"];
    const DROP: &[&str] = &["--continue", "-c"];
    const TAKES_VALUE: &[&str] = &[
        "--permission-mode", "--model", "--add-dir", "--allowedTools", "--allowed-tools", "--disallowedTools",
        "--disallowed-tools", "--append-system-prompt", "--system-prompt", "--agent", "--effort", "--settings",
        "--mcp-config", "--output-format", "--input-format", "--max-turns", "--fallback-model", "--tools",
    ];
    let mut out = Vec::new();
    let mut i = 1; // skip argv[0]
    while i < argv.len() {
        let a = argv[i].as_str();
        if DROP_WITH_VALUE.contains(&a) {
            i += 2;
            continue;
        }
        if DROP.contains(&a) {
            i += 1;
            continue;
        }
        if a.starts_with('-') {
            out.push(a.to_string());
            if TAKES_VALUE.contains(&a) && i + 1 < argv.len() {
                out.push(argv[i + 1].clone());
                i += 1;
            }
        }
        // positional (initial prompt): dropped
        i += 1;
    }
    out
}

// ---------- inventory ----------

#[derive(Clone, Debug)]
struct Agent {
    session: String,
    pane: String,
    tab: String,
    name: String,
    title: String,
    status: String,
    sid: String,
    cwd: String,
    idle: Option<f64>,
    rss_mb: u64,
    args: Vec<String>,
}

fn agents() -> Vec<Agent> {
    let rss = rss_by_pgid();
    let mut out = Vec::new();
    for s in sessions() {
        let (Ok(list), Ok(tabs)) = (herdr(Some(&s), &["agent", "list"]), herdr(Some(&s), &["tab", "list"])) else { continue };
        let mut tab_label: BTreeMap<String, String> = BTreeMap::new();
        for t in tabs.get("tabs").and_then(Value::as_array).into_iter().flatten() {
            if let (Some(id), Some(l)) = (t.get("tab_id").and_then(Value::as_str), t.get("label").and_then(Value::as_str)) {
                tab_label.insert(id.into(), l.into());
            }
        }
        for a in list.get("agents").and_then(Value::as_array).into_iter().flatten() {
            if a.get("agent").and_then(Value::as_str) != Some("claude") {
                continue;
            }
            let g = |k: &str| a.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            let pane = g("pane_id");
            let sid = a.get("agent_session").and_then(|x| x.get("value")).and_then(Value::as_str).unwrap_or("").to_string();
            let cwd = if g("cwd").is_empty() { home().to_string_lossy().into_owned() } else { g("cwd") };
            let procs = pane_procs(&s, &pane);
            let rss_mb = procs.pgid.and_then(|p| rss.get(&p)).map(|kb| kb / 1024).unwrap_or(0);
            out.push(Agent {
                session: s.clone(),
                tab: tab_label.get(&g("tab_id")).cloned().unwrap_or_else(|| g("tab_id")),
                name: g("name"),
                title: g("terminal_title_stripped"),
                status: if g("agent_status").is_empty() { "unknown".into() } else { g("agent_status") },
                idle: if sid.is_empty() { None } else { idle_hours(&cwd, &sid) },
                args: procs.claude_argv.as_deref().map(launch_flags).unwrap_or_default(),
                rss_mb,
                pane,
                sid,
                cwd,
            });
        }
    }
    out
}

fn sleepable(a: &Agent) -> bool {
    a.status == "idle" || a.status == "done"
}

// ---------- actions ----------

fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Park: a visible note, then an external wait. Enter resumes the chat in place.
/// Returns true when the pane ended up parked.
fn arm(session: &str, pane: &str, tab: &str, sid: &str, cwd: &str, args: &[String]) -> bool {
    // Typing into a parked pane feeds its wait and wakes the chat.
    if is_parked(session, pane) {
        return false;
    }
    // Typing while the old chat is still shutting down loses the text.
    if !wait_for_prompt(session, pane, 20) {
        log(&format!("skip arm {}: pane not at a shell prompt", key(session, pane)));
        return false;
    }
    let flags = args.iter().map(|a| shq(a)).collect::<Vec<_>>().join(" ");
    let cmd = format!(
        "clear; printf {} {}; {} >/dev/null; cd {} && claude --resume {} {}",
        shq(&format!("\\n\\n   zz  sleeping: %s\\n   {PARK_MARK}\\n\\n")),
        shq(tab),
        PARK_PROC,
        shq(cwd),
        sid,
        flags
    );
    let _ = hs(session, &["pane", "send-text", pane, &cmd]);
    let _ = hs(session, &["pane", "send-keys", pane, "Enter"]);
    for _ in 0..6 {
        sleep(Duration::from_millis(500));
        if is_parked(session, pane) {
            return true;
        }
    }
    false
}

fn sleep_one(a: &Agent, dry: bool) -> bool {
    let k = key(&a.session, &a.pane);
    if a.sid.is_empty() {
        log(&format!("skip {k}: no session id"));
        return false;
    }
    if !sleepable(a) {
        log(&format!("skip {k}: status={}", a.status));
        return false;
    }
    if dry {
        log(&format!("[dry] sleep {k} tab={} sid={} idle={} rss={}MB", a.tab, &a.sid[..8], fmt_h(a.idle), a.rss_mb));
        return true;
    }
    let _ = hs(&a.session, &["pane", "send-text", &a.pane, "/exit"]);
    sleep(Duration::from_millis(400));
    let _ = hs(&a.session, &["pane", "send-keys", &a.pane, "Enter"]);
    for _ in 0..40 {
        sleep(Duration::from_millis(500));
        if !pane_has_agent(&a.session, &a.pane) {
            break;
        }
    }
    if pane_has_agent(&a.session, &a.pane) {
        log(&format!("FAILED sleep {k}: agent still present after /exit (unsent text in the prompt?)"));
        return false;
    }
    let parked = arm(&a.session, &a.pane, &a.tab, &a.sid, &a.cwd, &a.args);
    let mut st = load_state();
    st.insert(
        k.clone(),
        Sleeping {
            session: a.session.clone(),
            pane: a.pane.clone(),
            tab: a.tab.clone(),
            name: if a.name.is_empty() { a.tab.clone() } else { a.name.clone() },
            sid: a.sid.clone(),
            cwd: a.cwd.clone(),
            args: a.args.clone(),
            slept_at: now_iso(),
            idle_hours: a.idle.unwrap_or(-1.0),
        },
    );
    save_state(&st);
    log(&format!(
        "slept {k} tab={} sid={} freed={}MB{}",
        a.tab,
        &a.sid[..8],
        a.rss_mb,
        if parked { "" } else { " (NOT parked, wake with the CLI)" }
    ));
    true
}

fn wake_one(k: &str, dry: bool) -> bool {
    let mut st = load_state();
    let Some(s) = st.get(k).cloned() else {
        eprintln!("not sleeping: {k}");
        return false;
    };
    if dry {
        log(&format!("[dry] wake {k} sid={} cwd={}", &s.sid[..8], s.cwd));
        return true;
    }
    if pane_has_agent(&s.session, &s.pane) {
        log(&format!("skip wake {k}: pane already has an agent"));
        st.remove(k);
        save_state(&st);
        return false;
    }
    // Parked pane: Enter resumes in place.
    let _ = hs(&s.session, &["pane", "send-keys", &s.pane, "Enter"]);
    if wait_for_agent(&s.session, &s.pane, 20) {
        st.remove(k);
        save_state(&st);
        log(&format!("woke {k} tab={} sid={} (Enter)", s.tab, &s.sid[..8]));
        return true;
    }
    // Plain prompt (someone hit Ctrl-C on the wait): start it ourselves.
    let _ = hs(&s.session, &["pane", "send-text", &s.pane, &format!("cd {}", shq(&s.cwd))]);
    let _ = hs(&s.session, &["pane", "send-keys", &s.pane, "Enter"]);
    if wait_for_agent(&s.session, &s.pane, 15) {
        st.remove(k);
        save_state(&st);
        log(&format!("woke {k} tab={} sid={} (parked, second Enter)", s.tab, &s.sid[..8]));
        return true;
    }
    let name: String = {
        let raw = if s.name.is_empty() { s.tab.clone() } else { s.name.clone() };
        let n: String = raw.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '-' }).take(40).collect();
        if n.is_empty() { "claude".into() } else { n }
    };
    let mut args: Vec<String> = vec![
        "agent".into(), "start".into(), name, "--kind".into(), "claude".into(), "--pane".into(), s.pane.clone(),
        "--timeout".into(), "120000".into(), "--".into(), "--resume".into(), s.sid.clone(),
    ];
    args.extend(s.args.iter().cloned());
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    match herdr(Some(&s.session), &refs) {
        Ok(_) => {
            st.remove(k);
            save_state(&st);
            log(&format!("woke {k} tab={} sid={} (agent start)", s.tab, &s.sid[..8]));
            true
        }
        Err(e) => {
            log(&format!("FAILED wake {k}: {e}"));
            false
        }
    }
}

/// Forget state for panes that woke on their own (someone pressed Enter).
fn reconcile(all: &[Agent]) -> State {
    let mut st = load_state();
    let before = st.len();
    st.retain(|k, _| !all.iter().any(|a| key(&a.session, &a.pane) == *k));
    if st.len() != before {
        save_state(&st);
    }
    st
}

// ---------- commands ----------

fn fmt_h(h: Option<f64>) -> String {
    match h {
        None => "?".into(),
        Some(h) if h >= 48.0 => format!("{:.1}d", h / 24.0),
        Some(h) => format!("{h:.1}h"),
    }
}

fn cmd_list(hours: f64) {
    let mut all = agents();
    let st = reconcile(&all);
    all.sort_by(|a, b| b.idle.unwrap_or(-1.0).partial_cmp(&a.idle.unwrap_or(-1.0)).unwrap());
    let mut rss = 0u64;
    println!("AWAKE");
    for a in &all {
        rss += a.rss_mb;
        let mark = if sleepable(a) && a.idle.map(|h| h >= hours).unwrap_or(false) { "*" } else { " " };
        let title: String = a.title.chars().take(40).collect();
        println!(
            "{mark} {:<20} {:<7} {:<8} idle={:<6} {:>4}MB  {}  {}  {}",
            a.session, a.pane, a.status, fmt_h(a.idle), a.rss_mb, a.tab, a.sid.get(..8).unwrap_or("-"), title
        );
    }
    println!("\n{} awake, {:.1} GB. * = idle >= {hours}h and would sleep", all.len(), rss as f64 / 1024.0);
    println!("\nSLEEPING ({})", st.len());
    for s in st.values() {
        println!("  {:<20} {:<7} since {}  {}  {}", s.session, s.pane, &s.slept_at[..16], s.tab, &s.sid[..8]);
    }
}

fn cmd_sleep(session: Option<String>, panes: Vec<String>, all_idle: bool, hours: f64, dry: bool) {
    let all = agents();
    let targets: Vec<Agent> = if all_idle {
        all.iter().filter(|a| sleepable(a) && a.idle.map(|h| h >= hours).unwrap_or(false)).cloned().collect()
    } else {
        let Some(session) = session.filter(|_| !panes.is_empty()) else {
            eprintln!("usage: herdr-sleep sleep <session> <pane>... | --all-idle [--hours N] [--dry-run]");
            std::process::exit(1);
        };
        let t: Vec<Agent> = all.iter().filter(|a| a.session == session && panes.contains(&a.pane)).cloned().collect();
        let st = load_state();
        for p in panes.iter().filter(|p| !t.iter().any(|x| &x.pane == *p)) {
            match st.get(&key(&session, p)) {
                Some(sl) if !dry => {
                    let did = arm(&sl.session, &sl.pane, &sl.tab, &sl.sid, &sl.cwd, &sl.args);
                    log(&format!("{} {}", if did { "armed" } else { "already parked" }, key(&session, p)));
                }
                _ => eprintln!("no claude agent on: {}", key(&session, p)),
            }
        }
        t
    };
    let (mut n, mut mb) = (0, 0u64);
    for a in &targets {
        if sleep_one(a, dry) {
            n += 1;
            mb += a.rss_mb;
        }
    }
    log(&format!("{}slept {n} of {}, {:.1} GB", if dry { "[dry] " } else { "" }, targets.len(), mb as f64 / 1024.0));
}

fn cmd_wake(session: Option<String>, panes: Vec<String>, all: bool, dry: bool) {
    let keys: Vec<String> = if all {
        load_state().keys().cloned().collect()
    } else {
        let Some(session) = session.filter(|_| !panes.is_empty()) else {
            eprintln!("usage: herdr-sleep wake <session> <pane>... | --all [--dry-run]");
            std::process::exit(1);
        };
        panes.iter().map(|p| key(&session, p)).collect()
    };
    let n = keys.iter().filter(|k| wake_one(k, dry)).count();
    log(&format!("{}woke {n} of {}", if dry { "[dry] " } else { "" }, keys.len()));
}

fn plist_path() -> PathBuf {
    home().join("Library").join("LaunchAgents").join(format!("{LAUNCHD_LABEL}.plist"))
}

fn cmd_install(hours: f64, every: &str, dry: bool) {
    if !cfg!(target_os = "macos") {
        eprintln!("install writes a launchd agent and only works on macOS. On Linux, add a cron or systemd timer that runs: herdr-sleep auto --hours {hours}");
        std::process::exit(1);
    }
    let secs: u64 = if let Some(h) = every.strip_suffix('h') {
        h.parse::<u64>().unwrap_or(1) * 3600
    } else {
        every.trim_end_matches('m').parse::<u64>().unwrap_or(15) * 60
    };
    let exe = std::env::current_exe().unwrap();
    let path = std::env::var("PATH").unwrap_or_else(|_| "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin".into());
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key><array>
    <string>{}</string><string>auto</string><string>--hours</string><string>{hours}</string>
  </array>
  <key>EnvironmentVariables</key><dict><key>PATH</key><string>{path}</string><key>HOME</key><string>{}</string></dict>
  <key>StartInterval</key><integer>{secs}</integer>
  <key>RunAtLoad</key><true/>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict></plist>
"#,
        exe.display(),
        home().display(),
        state_dir().join("launchd.out").display(),
        state_dir().join("launchd.err").display()
    );
    let plist = plist_path();
    if dry {
        println!("[dry] would write {}:\n{xml}", plist.display());
        return;
    }
    let _ = fs::create_dir_all(state_dir());
    let _ = fs::create_dir_all(plist.parent().unwrap());
    fs::write(&plist, xml).expect("write plist");
    let _ = Command::new("launchctl").args(["unload", plist.to_str().unwrap()]).output();
    let r = Command::new("launchctl").args(["load", plist.to_str().unwrap()]).output().expect("launchctl");
    println!(
        "installed {}: auto-sleep idle >= {hours}h every {every}. launchctl: {}",
        plist.display(),
        if r.status.success() { "ok".into() } else { String::from_utf8_lossy(&r.stderr).into_owned() }
    );
}

fn cmd_uninstall() {
    let plist = plist_path();
    let _ = Command::new("launchctl").args(["unload", plist.to_str().unwrap()]).output();
    let _ = fs::remove_file(&plist);
    println!("removed {}", plist.display());
}

fn main() {
    match Cli::parse().cmd {
        Cmd::List { hours } => cmd_list(hours),
        Cmd::Sleep { session, panes, all_idle, hours, dry_run } => cmd_sleep(session, panes, all_idle, hours, dry_run),
        Cmd::Wake { session, panes, all, dry_run } => cmd_wake(session, panes, all, dry_run),
        Cmd::Auto { hours, dry_run } => cmd_sleep(None, vec![], true, hours, dry_run),
        Cmd::Install { hours, every, dry_run } => cmd_install(hours, &every, dry_run),
        Cmd::Uninstall => cmd_uninstall(),
    }
}

// ---------- tests ----------

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn slug_matches_claude_code() {
        assert_eq!(slug("/Users/amir/.herdr/worktrees/slr-eval/feat-x"), "-Users-amir--herdr-worktrees-slr-eval-feat-x");
    }

    #[test]
    fn iso_roundtrip() {
        let t = parse_iso("2026-09-08T16:12:31.261Z").unwrap();
        assert_eq!(iso8601(t as u64), "2026-09-08T16:12:31Z");
        assert_eq!(parse_iso("garbage"), None);
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn launch_flags_drop_resume_and_prompt() {
        let argv = v(&["claude", "--resume", "abc", "--dangerously-skip-permissions", "Read docs/brief.md and start"]);
        assert_eq!(launch_flags(&argv), v(&["--dangerously-skip-permissions"]));
        let argv = v(&["/usr/local/bin/claude", "--permission-mode", "bypassPermissions", "--add-dir", "/tmp/x", "-c"]);
        assert_eq!(launch_flags(&argv), v(&["--permission-mode", "bypassPermissions", "--add-dir", "/tmp/x"]));
        assert_eq!(launch_flags(&v(&["claude"])), Vec::<String>::new());
    }

    #[test]
    fn last_message_ignores_bridge_rows() {
        let dir = std::env::temp_dir().join(format!("herdr-sleep-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("t.jsonl");
        fs::write(
            &f,
            concat!(
                "{\"type\":\"user\",\"timestamp\":\"2026-09-01T00:00:00.000Z\"}\n",
                "{\"type\":\"assistant\",\"timestamp\":\"2026-09-02T00:00:00.000Z\"}\n",
                "{\"type\":\"bridge-session\",\"sessionId\":\"x\"}\n",
                "{\"type\":\"system\",\"timestamp\":\"2026-09-08T00:00:00.000Z\"}\n",
                "{\"type\":\"bridge-session\",\"sessionId\":\"x\"}\n"
            ),
        )
        .unwrap();
        assert_eq!(last_message_secs(&f), parse_iso("2026-09-02T00:00:00Z"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn shell_detection() {
        assert!(is_shell("-zsh"));
        assert!(is_shell("/bin/bash"));
        assert!(!is_shell("head -n1"));
        assert!(!is_shell("claude --resume x"));
    }

    #[test]
    fn shell_quote() {
        assert_eq!(shq("it's"), "'it'\\''s'");
    }
}
