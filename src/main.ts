#!/usr/bin/env bun
// herdr-sleep: put idle Claude chats in herdr panes to sleep and wake them by id.
//
// sleep = send /exit to the chat. The pane, tab and shell stay. RAM drops to ~0.
// wake  = `herdr agent start ... -- --resume <id>` in the same pane.
// The chat itself lives in ~/.claude/projects/<cwd-slug>/<id>.jsonl, so nothing is lost.

import { closeSync, existsSync, mkdirSync, openSync, readFileSync, readSync, statSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

const HOME = homedir();
const STATE_DIR = join(HOME, ".config", "herdr-sleep");
const STATE = join(STATE_DIR, "sleeping.json");
const LOG = join(STATE_DIR, "auto.log");
const CLAUDE_PROJECTS = join(HOME, ".claude", "projects");
const DEFAULT_HOURS = 24;

type Sleeping = {
  session: string; pane: string; tab: string; name: string;
  sid: string; cwd: string; sleptAt: string; idleHours: number;
};
type Agent = {
  session: string; pane: string; tab: string; name: string; title: string;
  status: string; sid: string; cwd: string; idleHours: number | null; rssMb: number;
};

// ---------- herdr ----------

async function herdr(session: string | null, args: string[]): Promise<any> {
  const full = ["herdr", ...(session ? ["--session", session] : []), ...args];
  const p = Bun.spawn(full, { stdout: "pipe", stderr: "pipe" });
  const out = (await new Response(p.stdout).text()).trim();
  const err = (await new Response(p.stderr).text()).trim();
  await p.exited;
  if (out.startsWith("{")) {
    const o = JSON.parse(out);
    if (o.error) throw new Error(`${full.join(" ")}\n  -> ${o.error.message ?? JSON.stringify(o.error)}`);
    return o.result ?? o;
  }
  if (p.exitCode !== 0) throw new Error(`${full.join(" ")}\n  -> ${err || out}`);
  return { text: out };
}

async function sessions(): Promise<string[]> {
  const r = await herdr(null, ["session", "list", "--json"]);
  return (r.sessions ?? []).filter((s: any) => s.running).map((s: any) => s.name);
}

// ---------- state ----------

function loadState(): Record<string, Sleeping> {
  if (!existsSync(STATE)) return {};
  return JSON.parse(readFileSync(STATE, "utf8"));
}
function saveState(s: Record<string, Sleeping>) {
  mkdirSync(STATE_DIR, { recursive: true });
  writeFileSync(STATE, JSON.stringify(s, null, 2) + "\n");
}
function log(line: string) {
  mkdirSync(STATE_DIR, { recursive: true });
  const msg = `${new Date().toISOString()} ${line}`;
  console.log(msg);
  try { writeFileSync(LOG, msg + "\n", { flag: "a" }); } catch {}
}
const key = (session: string, pane: string) => `${session}/${pane}`;

// ---------- transcript idle time ----------

const slug = (cwd: string) => cwd.replace(/[^A-Za-z0-9]/g, "-");

function transcriptPath(cwd: string, sid: string): string | null {
  const direct = join(CLAUDE_PROJECTS, slug(cwd), `${sid}.jsonl`);
  if (existsSync(direct)) return direct;
  const hit = Bun.spawnSync(["sh", "-c", `ls ${CLAUDE_PROJECTS}/*/${sid}.jsonl 2>/dev/null | head -1`]);
  const f = new TextDecoder().decode(hit.stdout).trim();
  return f && existsSync(f) ? f : null;
}

// Last user/assistant message time. File mtime lies: Remote Control appends
// "bridge-session" rows on reconnect, which touches old files.
function transcriptIdleHours(cwd: string, sid: string): number | null {
  const f = transcriptPath(cwd, sid);
  if (!f) return null;
  const size = statSync(f).size;
  const chunk = 512 * 1024;
  let last: number | null = null;
  for (let end = size; end > 0 && last == null; end -= chunk) {
    const start = Math.max(0, end - chunk);
    const buf = Buffer.alloc(end - start);
    const fd = openSync(f, "r");
    readSync(fd, buf, 0, end - start, start);
    closeSync(fd);
    const lines = buf.toString("utf8").split("\n");
    for (let i = lines.length - 1; i >= 0; i--) {
      try {
        const o = JSON.parse(lines[i]);
        if ((o.type === "user" || o.type === "assistant") && o.timestamp) { last = Date.parse(o.timestamp); break; }
      } catch {}
    }
  }
  return last == null ? null : (Date.now() - last) / 3_600_000;
}

function rssByPgid(): Record<number, number> {
  const ps = Bun.spawnSync(["ps", "-axo", "rss,pgid"]);
  const out = new TextDecoder().decode(ps.stdout);
  const m: Record<number, number> = {};
  for (const line of out.split("\n")) {
    const mm = line.trim().match(/^(\d+)\s+(\d+)$/);
    if (mm) m[Number(mm[2])] = (m[Number(mm[2])] ?? 0) + Number(mm[1]);
  }
  return m;
}

async function paneRssMb(session: string, pane: string, rss: Record<number, number>): Promise<number> {
  try {
    const r = await herdr(session, ["pane", "process-info", "--pane", pane]);
    const pg = r.process_info?.foreground_process_group_id;
    return pg ? Math.round((rss[pg] ?? 0) / 1024) : 0;
  } catch { return 0; }
}

// ---------- inventory ----------

async function agents(): Promise<Agent[]> {
  const rss = rssByPgid();
  const out: Agent[] = [];
  for (const s of await sessions()) {
    let list: any, tabs: any;
    try {
      list = await herdr(s, ["agent", "list"]);
      tabs = await herdr(s, ["tab", "list"]);
    } catch { continue; }
    const tabLabel: Record<string, string> = {};
    for (const t of tabs.tabs ?? []) tabLabel[t.tab_id] = t.label;
    for (const a of list.agents ?? []) {
      if (a.agent !== "claude") continue;
      const sid = a.agent_session?.value ?? "";
      out.push({
        session: s, pane: a.pane_id, tab: tabLabel[a.tab_id] ?? a.tab_id, name: a.name ?? "",
        title: a.terminal_title_stripped ?? "", status: a.agent_status ?? "unknown",
        sid, cwd: a.cwd ?? HOME,
        idleHours: sid ? transcriptIdleHours(a.cwd ?? HOME, sid) : null,
        rssMb: await paneRssMb(s, a.pane_id, rss),
      });
    }
  }
  return out;
}

// ---------- actions ----------

async function paneHasAgent(session: string, pane: string): Promise<boolean> {
  const list = await herdr(session, ["agent", "list"]);
  return (list.agents ?? []).some((a: any) => a.pane_id === pane);
}

async function sleepOne(a: Agent, dry: boolean): Promise<boolean> {
  if (!a.sid) { log(`skip ${key(a.session, a.pane)}: no session id`); return false; }
  if (a.status !== "idle" && a.status !== "done") { log(`skip ${key(a.session, a.pane)}: status=${a.status}`); return false; }
  if (dry) { log(`[dry] sleep ${key(a.session, a.pane)} tab=${a.tab} sid=${a.sid.slice(0, 8)} idle=${a.idleHours?.toFixed(1)}h rss=${a.rssMb}MB`); return true; }

  await herdr(a.session, ["pane", "send-text", a.pane, "/exit"]);
  await Bun.sleep(400);
  await herdr(a.session, ["pane", "send-keys", a.pane, "Enter"]);
  for (let i = 0; i < 40; i++) {
    await Bun.sleep(500);
    if (!(await paneHasAgent(a.session, a.pane))) break;
  }
  if (await paneHasAgent(a.session, a.pane)) {
    log(`FAILED sleep ${key(a.session, a.pane)}: agent still present after /exit`);
    return false;
  }
  const st = loadState();
  st[key(a.session, a.pane)] = {
    session: a.session, pane: a.pane, tab: a.tab, name: a.name || a.tab, sid: a.sid, cwd: a.cwd,
    sleptAt: new Date().toISOString(), idleHours: a.idleHours ?? -1,
  };
  saveState(st);
  log(`slept ${key(a.session, a.pane)} tab=${a.tab} sid=${a.sid.slice(0, 8)} freed=${a.rssMb}MB`);
  return true;
}

async function wakeOne(k: string, dry: boolean): Promise<boolean> {
  const st = loadState();
  const s = st[k];
  if (!s) { console.error(`not sleeping: ${k}`); return false; }
  if (dry) { log(`[dry] wake ${k} sid=${s.sid.slice(0, 8)} cwd=${s.cwd}`); return true; }
  if (await paneHasAgent(s.session, s.pane)) {
    log(`skip wake ${k}: pane already has an agent`);
    delete st[k]; saveState(st);
    return false;
  }
  // Put the shell in the chat's original cwd so --resume finds the same project.
  await herdr(s.session, ["pane", "send-text", s.pane, `cd ${JSON.stringify(s.cwd)}`]);
  await herdr(s.session, ["pane", "send-keys", s.pane, "Enter"]);
  await Bun.sleep(300);
  const name = (s.name || s.tab || "claude").replace(/[^A-Za-z0-9._-]+/g, "-").slice(0, 40) || "claude";
  await herdr(s.session, [
    "agent", "start", name, "--kind", "claude", "--pane", s.pane, "--timeout", "120000",
    "--", "--resume", s.sid, "--dangerously-skip-permissions",
  ]);
  delete st[k]; saveState(st);
  log(`woke ${k} tab=${s.tab} sid=${s.sid.slice(0, 8)}`);
  return true;
}

// ---------- commands ----------

const argv = process.argv.slice(2);
const cmd = argv[0] ?? "help";
const flag = (n: string) => argv.includes(n);
const opt = (n: string, d: string) => { const i = argv.indexOf(n); return i >= 0 && argv[i + 1] ? argv[i + 1] : d; };
const hours = Number(opt("--hours", String(DEFAULT_HOURS)));
const dry = flag("--dry-run");
const positional = argv.slice(1).filter((a) => !a.startsWith("--") && a !== opt("--hours", "") && a !== opt("--every", ""));

const fmtH = (h: number | null) => h == null ? "?" : h >= 48 ? `${(h / 24).toFixed(1)}d` : `${h.toFixed(1)}h`;

async function cmdList() {
  const all = await agents();
  const st = loadState();
  all.sort((a, b) => (b.idleHours ?? -1) - (a.idleHours ?? -1));
  let rss = 0;
  console.log("AWAKE");
  for (const a of all) {
    rss += a.rssMb;
    const mark = a.idleHours != null && a.idleHours >= hours && (a.status === "idle" || a.status === "done") ? "*" : " ";
    console.log(`${mark} ${a.session.padEnd(20)} ${a.pane.padEnd(7)} ${a.status.padEnd(8)} idle=${fmtH(a.idleHours).padEnd(6)} ${String(a.rssMb).padStart(4)}MB  ${a.tab}  ${a.sid.slice(0, 8)}  ${a.title.slice(0, 40)}`);
  }
  console.log(`\n${all.length} awake, ${(rss / 1024).toFixed(1)} GB. * = idle >= ${hours}h, would sleep`);
  const sl = Object.values(st);
  console.log(`\nSLEEPING (${sl.length})`);
  for (const s of sl) console.log(`  ${s.session.padEnd(20)} ${s.pane.padEnd(7)} since ${s.sleptAt.slice(0, 16)}  ${s.tab}  ${s.sid.slice(0, 8)}`);
}

async function cmdSleep() {
  const all = await agents();
  let targets: Agent[];
  if (flag("--all-idle")) {
    targets = all.filter((a) => (a.status === "idle" || a.status === "done") && a.idleHours != null && a.idleHours >= hours);
  } else {
    const [session, ...panes] = positional;
    if (!session || !panes.length) { console.error("usage: herdr-sleep sleep <session> <pane>... | --all-idle [--hours N] [--dry-run]"); process.exit(1); }
    targets = all.filter((a) => a.session === session && panes.includes(a.pane));
    const missing = panes.filter((p) => !targets.some((t) => t.pane === p));
    if (missing.length) console.error(`no claude agent on: ${missing.join(", ")}`);
  }
  let n = 0, mb = 0;
  for (const a of targets) { if (await sleepOne(a, dry)) { n++; mb += a.rssMb; } }
  log(`${dry ? "[dry] " : ""}slept ${n} of ${targets.length}, ${(mb / 1024).toFixed(1)} GB`);
}

async function cmdWake() {
  const st = loadState();
  let keys: string[];
  if (flag("--all")) keys = Object.keys(st);
  else {
    const [session, ...panes] = positional;
    if (!session || !panes.length) { console.error("usage: herdr-sleep wake <session> <pane>... | --all [--dry-run]"); process.exit(1); }
    keys = panes.map((p) => key(session, p));
  }
  let n = 0;
  for (const k of keys) { try { if (await wakeOne(k, dry)) n++; } catch (e: any) { log(`FAILED wake ${k}: ${e.message}`); } }
  log(`${dry ? "[dry] " : ""}woke ${n} of ${keys.length}`);
}

async function cmdInstall() {
  const every = opt("--every", "15m");
  const secs = every.endsWith("h") ? Number(every.slice(0, -1)) * 3600 : Number(every.replace(/m$/, "")) * 60;
  const bunPath = process.execPath;
  const script = join(import.meta.dir, "main.ts");
  const label = "com.amir.herdr-sleep";
  const plist = join(HOME, "Library", "LaunchAgents", `${label}.plist`);
  const path = process.env.PATH ?? "/usr/local/bin:/usr/bin:/bin";
  const xml = `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>${label}</string>
  <key>ProgramArguments</key><array>
    <string>${bunPath}</string><string>run</string><string>${script}</string>
    <string>auto</string><string>--hours</string><string>${hours}</string>
  </array>
  <key>EnvironmentVariables</key><dict><key>PATH</key><string>${path}</string><key>HOME</key><string>${HOME}</string></dict>
  <key>StartInterval</key><integer>${secs}</integer>
  <key>RunAtLoad</key><true/>
  <key>StandardOutPath</key><string>${join(STATE_DIR, "launchd.out")}</string>
  <key>StandardErrorPath</key><string>${join(STATE_DIR, "launchd.err")}</string>
</dict></plist>
`;
  mkdirSync(STATE_DIR, { recursive: true });
  if (dry) { console.log(`[dry] would write ${plist}:\n${xml}`); return; }
  writeFileSync(plist, xml);
  Bun.spawnSync(["launchctl", "unload", plist]);
  const r = Bun.spawnSync(["launchctl", "load", plist]);
  console.log(`installed ${plist}: auto-sleep idle >= ${hours}h every ${every}. launchctl: ${r.exitCode === 0 ? "ok" : new TextDecoder().decode(r.stderr)}`);
}

async function cmdUninstall() {
  const plist = join(HOME, "Library", "LaunchAgents", "com.amir.herdr-sleep.plist");
  Bun.spawnSync(["launchctl", "unload", plist]);
  if (existsSync(plist)) Bun.spawnSync(["rm", plist]);
  console.log(`removed ${plist}`);
}

function help() {
  console.log(`herdr-sleep: free RAM from idle Claude chats in herdr, keep them resumable.

  list   [--hours N]                       all chats, idle time, RAM, who would sleep
  sleep  <session> <pane>... [--dry-run]   exit these chats, keep panes
  sleep  --all-idle [--hours N] [--dry-run] exit every idle chat older than N hours (default ${DEFAULT_HOURS})
  wake   <session> <pane>... [--dry-run]   resume these chats in their panes
  wake   --all                             resume every sleeping chat
  auto   [--hours N] [--dry-run]           one auto-sleep pass (what the timer runs)
  install [--hours N] [--every 15m]        launchd timer running auto
  uninstall                                remove the timer

state: ${STATE}
log:   ${LOG}`);
}

switch (cmd) {
  case "list": await cmdList(); break;
  case "sleep": await cmdSleep(); break;
  case "wake": await cmdWake(); break;
  case "auto": argv.push("--all-idle"); await cmdSleep(); break;
  case "install": await cmdInstall(); break;
  case "uninstall": await cmdUninstall(); break;
  default: help();
}
