// herdr-chat-search: shared core (index, search, sessions, actions)
// Created 2026-11-08. Used by the terminal UI (tui.rs), the browser panel (panel.rs)
// and the command line (main.rs).
//
// How it works
//   * `herdr agent list` gives each agent's Claude session id.
//   * Claude Code keeps every chat as ~/.claude/projects/<project>/<session-id>.jsonl.
//   * For each session we keep a small plain-text index in the cache directory:
//       <id>.txt   one line per chat message: timestamp TAB role TAB text
//       <id>.off   byte offset up to which the .jsonl has been read (incremental update)
//       <id>.meta  JSON with the session title and working directory
//     A search then scans only the plain chat text instead of hundreds of MB of JSON.

use chrono::{DateTime, Local, TimeZone, Utc};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub const MAX_ROWS: usize = 300;
pub const AGE_DAYS: [u32; 7] = [0, 3, 7, 14, 30, 60, 90];

// ───────────────────────── language ─────────────────────────

pub fn de() -> bool {
    std::env::var("HERDR_CHAT_SEARCH_LANG")
        .or_else(|_| std::env::var("LC_ALL"))
        .or_else(|_| std::env::var("LANG"))
        .map(|l| l.starts_with("de"))
        .unwrap_or(false)
}

pub fn t<'a>(german: &'a str, english: &'a str) -> &'a str {
    if de() {
        german
    } else {
        english
    }
}

pub fn age_label(days: u32) -> String {
    match days {
        0 => t("nur offene", "open only").to_string(),
        d => format!("{} {}", d, t("T", "d")),
    }
}

// ───────────────────────── paths and config ─────────────────────────

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
}

pub fn projects_dir() -> PathBuf {
    match std::env::var("CLAUDE_CONFIG_DIR") {
        Ok(d) if !d.is_empty() => PathBuf::from(d).join("projects"),
        _ => home().join(".claude").join("projects"),
    }
}

pub fn cache_dir() -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".cache"));
    base.join("herdr-chat-search")
}

pub fn herdr_bin() -> String {
    std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into())
}

#[derive(Clone, Debug)]
pub struct Config {
    /// Command that reopens a finished Claude session; `{id}` is replaced by the session id.
    pub resume_command: String,
    /// Extra Host names the browser panel accepts (for a reverse proxy name such as chats.example).
    pub extra_hosts: Vec<String>,
}

pub fn load_config() -> Config {
    let mut c = Config {
        resume_command: "claude --resume {id}".into(),
        extra_hosts: vec![],
    };
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".config"));
    let file = base.join("herdr/plugins/config/herdr-chat-search/config.toml");
    if let Ok(text) = fs::read_to_string(file) {
        if let Ok(v) = text.parse::<toml::Value>() {
            if let Some(s) = v.get("resume_command").and_then(|x| x.as_str()) {
                c.resume_command = s.to_string();
            }
            if let Some(a) = v.get("extra_hosts").and_then(|x| x.as_array()) {
                c.extra_hosts = a.iter().filter_map(|h| h.as_str()).map(|h| h.to_lowercase()).collect();
            }
        }
    }
    if let Ok(s) = std::env::var("HERDR_CHAT_SEARCH_RESUME_CMD") {
        if !s.trim().is_empty() {
            c.resume_command = s;
        }
    }
    c
}

pub fn resume_words(cfg: &Config, sid: &str) -> Vec<String> {
    cfg.resume_command
        .replace("{id}", sid)
        .split_whitespace()
        .map(String::from)
        .collect()
}

pub fn valid_sid(sid: &str) -> bool {
    sid.len() == 36
        && sid
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == '-')
}

// ───────────────────────── herdr CLI ─────────────────────────

pub fn herdr_json(args: &[&str]) -> Value {
    Command::new(herdr_bin())
        .args(args)
        .output()
        .ok()
        .and_then(|o| serde_json::from_slice(&o.stdout).ok())
        .unwrap_or(Value::Null)
}

pub fn find_key<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Object(m) => {
            if let Some(x) = m.get(key) {
                return Some(x);
            }
            m.values().find_map(|x| find_key(x, key))
        }
        Value::Array(a) => a.iter().find_map(|x| find_key(x, key)),
        _ => None,
    }
}

// ───────────────────────── sessions ─────────────────────────

#[derive(Clone, Debug)]
pub struct Session {
    pub live: bool,
    pub agent: String,
    pub pane: String,
    pub sid: String,
    pub file: Option<PathBuf>,
    pub cwd: String,
    pub title: String,
    pub status: String,
    pub ws: String,
    pub mtime: f64,
}

pub struct FileInfo {
    pub path: PathBuf,
    pub mtime: f64,
}

fn epoch(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

pub fn now() -> f64 {
    epoch(SystemTime::now())
}

/// Every session file under ~/.claude/projects. About 100 ids exist as a copy in two
/// project folders; per id only the newest file counts.
pub fn scan_files() -> HashMap<String, FileInfo> {
    let mut out: HashMap<String, FileInfo> = HashMap::new();
    let Ok(dirs) = fs::read_dir(projects_dir()) else {
        return out;
    };
    for d in dirs.flatten() {
        let Ok(files) = fs::read_dir(d.path()) else { continue };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(sid) = p.file_stem().and_then(|s| s.to_str()).map(String::from) else {
                continue;
            };
            let Ok(md) = f.metadata() else { continue };
            let mt = md.modified().map(epoch).unwrap_or(0.0);
            if out.get(&sid).map(|x| x.mtime >= mt).unwrap_or(false) {
                continue;
            }
            out.insert(sid, FileInfo { path: p, mtime: mt });
        }
    }
    out
}

fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("")
}

pub fn live_sessions(files: &HashMap<String, FileInfo>) -> Vec<Session> {
    let agents = herdr_json(&["agent", "list"]);
    let labels: HashMap<String, String> = herdr_json(&["workspace", "list"])
        .pointer("/result/workspaces")
        .and_then(|w| w.as_array())
        .map(|a| {
            a.iter()
                .map(|w| (str_of(w, "workspace_id").to_string(), str_of(w, "label").to_string()))
                .collect()
        })
        .unwrap_or_default();
    let mut out = vec![];
    let list = agents.pointer("/result/agents").and_then(|a| a.as_array());
    for a in list.into_iter().flatten() {
        let sid = a
            .pointer("/agent_session/value")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let agent = str_of(a, "agent").to_string();
        let fi = if agent == "claude" && !sid.is_empty() {
            files.get(&sid)
        } else {
            None
        };
        let mut title = str_of(a, "terminal_title_stripped").to_string();
        if let Some(i) = title.rfind(" | ") {
            title.truncate(i); // herdr appends " | <workspace>"
        }
        out.push(Session {
            live: true,
            agent: if agent.is_empty() { "?".into() } else { agent },
            pane: str_of(a, "pane_id").to_string(),
            sid,
            file: fi.map(|f| f.path.clone()),
            cwd: str_of(a, "cwd").to_string(),
            title,
            status: str_of(a, "agent_status").to_string(),
            ws: labels.get(str_of(a, "workspace_id")).cloned().unwrap_or_default(),
            mtime: fi.map(|f| f.mtime).unwrap_or_else(now),
        });
    }
    out
}

static LIVE_CACHE: Mutex<Option<(Instant, Vec<Session>)>> = Mutex::new(None);

/// `herdr agent list` costs ~100 ms; keystrokes should not call it every time.
pub fn live_cached(files: &HashMap<String, FileInfo>) -> Vec<Session> {
    if let Ok(g) = LIVE_CACHE.lock() {
        if let Some((t0, v)) = g.as_ref() {
            if t0.elapsed().as_secs_f32() < 2.0 {
                return v.clone();
            }
        }
    }
    let v = live_sessions(files);
    if let Ok(mut g) = LIVE_CACHE.lock() {
        *g = Some((Instant::now(), v.clone()));
    }
    v
}

pub fn old_sessions(files: &HashMap<String, FileInfo>, age_days: u32, exclude: &HashSet<String>) -> Vec<Session> {
    let cutoff = now() - age_days as f64 * 86400.0;
    files
        .iter()
        .filter(|(sid, fi)| !exclude.contains(*sid) && fi.mtime >= cutoff)
        .map(|(sid, fi)| Session {
            live: false,
            agent: "claude".into(),
            pane: String::new(),
            sid: sid.clone(),
            file: Some(fi.path.clone()),
            cwd: String::new(),
            title: String::new(),
            status: String::new(),
            ws: String::new(),
            mtime: fi.mtime,
        })
        .collect()
}

// ───────────────────────── index ─────────────────────────

fn strip_system_reminders(s: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(OPEN) {
        match rest[i + OPEN.len()..].find(CLOSE) {
            Some(j) => {
                out.push_str(&rest[..i]);
                rest = &rest[i + OPEN.len() + j + CLOSE.len()..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

fn text_of(o: &Value) -> String {
    let c = o.pointer("/message/content");
    let raw = match c {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .filter(|b| b.get("type").and_then(|x| x.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|x| x.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return String::new(),
    };
    strip_system_reminders(&raw)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Default)]
struct Meta {
    title: String,
    cwd: String,
}

fn read_meta(sid: &str) -> Meta {
    let p = cache_dir().join(format!("{sid}.meta"));
    let v: Value = fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    Meta {
        title: str_of(&v, "title").to_string(),
        cwd: str_of(&v, "cwd").to_string(),
    }
}

/// Brings the index of one session up to date. Only the new part of the .jsonl is read.
fn index_one(path: &Path) -> std::io::Result<()> {
    let Some(sid) = path.file_stem().and_then(|s| s.to_str()) else {
        return Ok(());
    };
    let dir = cache_dir();
    let (txt, off, meta_p) = (
        dir.join(format!("{sid}.txt")),
        dir.join(format!("{sid}.off")),
        dir.join(format!("{sid}.meta")),
    );
    let size = fs::metadata(path)?.len();
    let mut done: u64 = fs::read_to_string(&off)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    // A session indexed by an older version has no .meta yet: rebuild it once.
    if done > size || (done > 0 && (!txt.exists() || !meta_p.exists())) {
        done = 0; // file was rewritten
        let _ = fs::remove_file(&txt);
        let _ = fs::remove_file(&meta_p);
    }
    if done == size {
        return Ok(());
    }
    let mut r = BufReader::new(File::open(path)?);
    r.seek(SeekFrom::Start(done))?;
    let mut meta = read_meta(sid);
    let mut meta_changed = false;
    let mut out = String::new();
    let mut pos = done;
    let mut line: Vec<u8> = vec![];
    loop {
        line.clear();
        let n = r.read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        if !line.ends_with(b"\n") {
            break; // line is still being written
        }
        pos += n as u64;
        let head = &line[..line.len().min(400)];
        let is_chat = line.starts_with(b"{\"parentUuid\"")
            && head.windows(11).any(|w| w == b"\"message\":{");
        if is_chat {
            let Ok(o) = serde_json::from_slice::<Value>(&line) else { continue };
            let ty = str_of(&o, "type");
            if (ty != "user" && ty != "assistant") || o.get("isMeta").and_then(|x| x.as_bool()) == Some(true) {
                continue;
            }
            if meta.cwd.is_empty() {
                let c = str_of(&o, "cwd");
                if !c.is_empty() {
                    meta.cwd = c.to_string();
                    meta_changed = true;
                }
            }
            let text = text_of(&o);
            if !text.is_empty() {
                out.push_str(&format!("{}\t{}\t{}\n", str_of(&o, "timestamp"), ty, text));
            }
        } else if line.starts_with(b"{\"type\":\"ai-title\"") {
            if let Ok(o) = serde_json::from_slice::<Value>(&line) {
                let ti = str_of(&o, "aiTitle");
                if !ti.is_empty() && ti != meta.title {
                    meta.title = ti.to_string();
                    meta_changed = true;
                }
            }
        }
    }
    fs::create_dir_all(&dir)?;
    {
        // Always create the file, even when empty, so "indexed but no chat" is not rebuilt each time.
        use std::io::Write;
        fs::OpenOptions::new().create(true).append(true).open(&txt)?.write_all(out.as_bytes())?;
    }
    if meta_changed || !meta_p.exists() {
        let v = serde_json::json!({"title": meta.title, "cwd": meta.cwd});
        fs::write(&meta_p, v.to_string())?;
    }
    fs::write(&off, pos.to_string())?;
    Ok(())
}

/// Updates the index of all given session files, in parallel.
pub fn ensure_index(files: &[PathBuf]) {
    let _ = fs::create_dir_all(cache_dir());
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8);
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(f) = files.get(i) else { break };
                let _ = index_one(f);
            });
        }
    });
}

pub fn index_size_mb() -> f64 {
    let Ok(rd) = fs::read_dir(cache_dir()) else { return 0.0 };
    rd.flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("txt"))
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum::<u64>() as f64
        / 1e6
}

// ───────────────────────── search ─────────────────────────

#[derive(Clone, Debug)]
pub struct Hit {
    pub ts: String,
    pub role: String,
    pub text: String,
}

#[derive(Default)]
pub struct FileHits {
    pub n: usize,
    pub hits: VecDeque<Hit>,
}

fn read_index(sid: &str) -> String {
    fs::read(cache_dir().join(format!("{sid}.txt")))
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

fn scan_index(sid: &str, words: &[String], keep: usize) -> Option<FileHits> {
    let content = read_index(sid);
    if content.is_empty() {
        return None;
    }
    let low = content.to_lowercase();
    let mut fh = FileHits::default();
    for (lo, orig) in low.lines().zip(content.lines()) {
        if !words.iter().all(|w| lo.contains(w.as_str())) {
            continue;
        }
        let mut p = orig.splitn(3, '\t');
        let (Some(ts), Some(role), Some(text)) = (p.next(), p.next(), p.next()) else { continue };
        fh.n += 1;
        if fh.hits.len() == keep {
            fh.hits.pop_front();
        }
        fh.hits.push_back(Hit { ts: ts.into(), role: role.into(), text: text.into() });
    }
    (fh.n > 0).then_some(fh)
}

/// Hits per session id: number of matching messages plus the newest `keep` of them.
/// A message matches when it contains ALL words (case-insensitive).
pub fn collect_hits(sids: &[String], words: &[String], keep: usize) -> HashMap<String, FileHits> {
    let next = AtomicUsize::new(0);
    let results: Mutex<HashMap<String, FileHits>> = Mutex::new(HashMap::new());
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8);
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(sid) = sids.get(i) else { break };
                if let Some(fh) = scan_index(sid, words, keep) {
                    if let Ok(mut g) = results.lock() {
                        g.insert(sid.clone(), fh);
                    }
                }
            });
        }
    });
    results.into_inner().unwrap_or_default()
}

pub fn fill_meta(s: &mut Session) {
    if s.sid.is_empty() {
        if s.title.is_empty() {
            s.title = t("(ohne Titel)", "(untitled)").into();
        }
        return;
    }
    let m = read_meta(&s.sid);
    let generic = s.title.is_empty() || s.title.eq_ignore_ascii_case("claude") || s.title.eq_ignore_ascii_case("claude code");
    if !s.live || generic {
        if !m.title.is_empty() {
            s.title = m.title.clone();
        } else if s.title.is_empty() {
            s.title = t("(ohne Titel)", "(untitled)").into();
        }
    }
    if s.cwd.is_empty() {
        s.cwd = m.cwd;
    }
    if s.ws.is_empty() && !s.cwd.is_empty() {
        s.ws = Path::new(&s.cwd).file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
    }
}

pub struct Row {
    pub s: Session,
    pub n: usize,
    pub text: String,
}

fn iso_of(epoch_s: f64) -> String {
    Utc.timestamp_opt(epoch_s as i64, 0)
        .single()
        .map(|d| d.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
        .unwrap_or_default()
}

pub fn split_words(q: &str) -> Vec<String> {
    q.to_lowercase().split_whitespace().map(String::from).collect()
}

/// The main search. `age` = 0: only open agents; otherwise also closed Claude sessions
/// that changed within the last `age` days.
pub fn search(q: &str, age: u32) -> Vec<Row> {
    let words = split_words(q);
    let files = scan_files();
    let live = live_cached(&files);
    let mut sessions = live.clone();
    if age > 0 {
        let exclude: HashSet<String> = live.iter().filter(|s| !s.sid.is_empty()).map(|s| s.sid.clone()).collect();
        sessions.extend(old_sessions(&files, age, &exclude));
    }
    let paths: Vec<PathBuf> = sessions.iter().filter_map(|s| s.file.clone()).collect();
    ensure_index(&paths);
    for s in sessions.iter_mut() {
        fill_meta(s);
    }
    if words.is_empty() {
        sessions.sort_by(|a, b| b.mtime.partial_cmp(&a.mtime).unwrap_or(std::cmp::Ordering::Equal));
        sessions.truncate(MAX_ROWS);
        return sessions.into_iter().map(|s| Row { s, n: 0, text: String::new() }).collect();
    }
    let sids: Vec<String> = sessions.iter().filter(|s| s.file.is_some()).map(|s| s.sid.clone()).collect();
    let hits = collect_hits(&sids, &words, 10);
    let mut scored: Vec<(bool, String, Row)> = vec![];
    for s in sessions {
        let h = if s.file.is_some() { hits.get(&s.sid) } else { None };
        // Codex and similar agents: herdr reports no session id, and `agent read` takes
        // 8-23 s per agent, so they are matched by title/workspace only.
        let meta = format!("{} {} {}", s.title, s.ws, s.cwd).to_lowercase();
        let title_hit = words.iter().all(|w| meta.contains(w.as_str()));
        if h.is_none() && !title_hit {
            continue;
        }
        let (n, text, key) = match h.and_then(|h| h.hits.back().map(|l| (h.n, l))) {
            Some((n, last)) => (n, last.text.clone(), if last.ts.is_empty() { iso_of(s.mtime) } else { last.ts.clone() }),
            None => (0, String::new(), iso_of(s.mtime)),
        };
        scored.push((n > 0, key, Row { s, n, text }));
    }
    scored.sort_by(|a, b| (b.0, &b.1).cmp(&(a.0, &a.1)));
    scored.truncate(MAX_ROWS);
    scored.into_iter().map(|x| x.2).collect()
}

pub enum Preview {
    NoLog,
    Hits { n: usize, hits: Vec<Hit> },
    Recent(Vec<Hit>),
}

pub fn preview(sid: &str, has_file: bool, words: &[String]) -> Preview {
    if !has_file || sid.is_empty() {
        return Preview::NoLog;
    }
    if words.is_empty() {
        let content = read_index(sid);
        let lines: Vec<&str> = content.lines().collect();
        let start = lines.len().saturating_sub(6);
        let hits = lines[start..]
            .iter()
            .filter_map(|l| {
                let mut p = l.splitn(3, '\t');
                Some(Hit { ts: p.next()?.into(), role: p.next()?.into(), text: p.next()?.into() })
            })
            .collect();
        return Preview::Recent(hits);
    }
    match scan_index(sid, words, 10) {
        Some(fh) => Preview::Hits { n: fh.n, hits: fh.hits.into_iter().rev().collect() },
        None => Preview::Hits { n: 0, hits: vec![] },
    }
}

// ───────────────────────── text helpers ─────────────────────────

/// Lower-cases per character, so positions stay aligned with the original text.
pub fn lower_chars(s: &str) -> Vec<char> {
    s.chars().map(|c| c.to_lowercase().next().unwrap_or(c)).collect()
}

fn find_sub(h: &[char], n: &[char], from: usize) -> Option<usize> {
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    (from..=h.len() - n.len()).find(|&i| h[i..i + n.len()] == *n)
}

/// Character ranges of all word matches (merged), for highlighting.
pub fn match_ranges(text: &str, words: &[String]) -> Vec<(usize, usize)> {
    let low = lower_chars(text);
    let mut r: Vec<(usize, usize)> = vec![];
    for w in words {
        let wc = lower_chars(w);
        let mut i = 0;
        while let Some(p) = find_sub(&low, &wc, i) {
            r.push((p, p + wc.len()));
            i = p + wc.len();
        }
    }
    r.sort();
    let mut merged: Vec<(usize, usize)> = vec![];
    for (a, b) in r {
        match merged.last_mut() {
            Some(l) if a <= l.1 => l.1 = l.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    merged
}

pub fn snippet(text: &str, word: &str, width: usize) -> String {
    let chars: Vec<char> = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().collect();
    let low: Vec<char> = chars.iter().map(|c| c.to_lowercase().next().unwrap_or(*c)).collect();
    let start = match find_sub(&low, &lower_chars(word), 0) {
        Some(i) => i.saturating_sub(width / 3),
        None => 0,
    };
    let s: String = chars.iter().skip(start).take(width).collect();
    if start > 0 {
        format!("…{s}")
    } else {
        s
    }
}

pub fn fmt_ts(ts: &str) -> String {
    DateTime::parse_from_rfc3339(ts)
        .map(|d| d.with_timezone(&Local).format("%d.%m. %H:%M").to_string())
        .unwrap_or_else(|_| "?".into())
}

pub fn fmt_age(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    if s < 3600 {
        format!("{} {}", (s / 60).max(1), t("Min", "min"))
    } else if s < 86400 {
        format!("{} {}", s / 3600, t("Std", "h"))
    } else {
        format!("{} {}", s / 86400, t("T", "d"))
    }
}

// ───────────────────────── actions ─────────────────────────

/// Jumps to an open agent, or reopens a closed session in a new Herdr workspace.
/// Returns the commands it ran, so the caller can show them.
pub fn open_in_herdr(s: &Session, cfg: &Config) -> Result<Vec<String>, String> {
    let hb = herdr_bin();
    if s.live {
        let st = Command::new(&hb).args(["agent", "focus", &s.pane]).status().map_err(|e| e.to_string())?;
        let log = format!("{hb} agent focus {}", s.pane);
        return if st.success() { Ok(vec![log]) } else { Err(format!("{log}: failed")) };
    }
    let cwd = if s.cwd.is_empty() { home().to_string_lossy().into_owned() } else { s.cwd.clone() };
    // Untitled sessions carry a placeholder like "(untitled)"; the id prefix is a better label.
    let label: String = (if s.title.is_empty() || s.title.starts_with('(') { &s.sid[..8.min(s.sid.len())] } else { &s.title }).chars().take(40).collect();
    let r = herdr_json(&["workspace", "create", "--cwd", &cwd, "--label", &label, "--focus"]);
    let pane = find_key(&r, "pane_id")
        .and_then(|x| x.as_str())
        .ok_or_else(|| format!("could not read pane id from: {}", r.to_string().chars().take(300).collect::<String>()))?
        .to_string();
    let words = resume_words(cfg, &s.sid);
    let mut args: Vec<&str> = vec!["pane", "run", &pane];
    args.extend(words.iter().map(|w| w.as_str()));
    herdr_json(&args);
    Ok(vec![
        format!("{hb} workspace create --cwd {cwd} --label \"{label}\" --focus"),
        format!("{hb} pane run {pane} {}", words.join(" ")),
    ])
}

/// Looks up one session by id (open agent first, else a closed session file).
pub fn session_by_sid(sid: &str) -> Option<Session> {
    let files = scan_files();
    let mut s = live_cached(&files).into_iter().find(|s| s.sid == sid).or_else(|| {
        files.get(sid).map(|fi| Session {
            live: false,
            agent: "claude".into(),
            pane: String::new(),
            sid: sid.into(),
            file: Some(fi.path.clone()),
            cwd: String::new(),
            title: String::new(),
            status: String::new(),
            ws: String::new(),
            mtime: fi.mtime,
        })
    })?;
    fill_meta(&mut s);
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_closed_system_reminders_only() {
        assert_eq!(strip_system_reminders("a<system-reminder>x</system-reminder>b"), "ab");
        // An unclosed tag stays in the text instead of swallowing everything after it.
        assert_eq!(strip_system_reminders("a<system-reminder>x"), "a<system-reminder>x");
    }

    #[test]
    fn text_of_joins_text_blocks_and_skips_tool_blocks() {
        let o: Value = serde_json::json!({"message": {"content": [
            {"type": "text", "text": "hello   world"},
            {"type": "tool_use", "name": "Bash"},
            {"type": "text", "text": "second"}]}});
        assert_eq!(text_of(&o), "hello world second");
        let s: Value = serde_json::json!({"message": {"content": "plain  text"}});
        assert_eq!(text_of(&s), "plain text");
    }

    #[test]
    fn match_ranges_are_case_insensitive_and_merged() {
        let r = match_ranges("Herdr and herdrx", &["herdr".to_string()]);
        assert_eq!(r, vec![(0, 5), (10, 15)]);
        let merged = match_ranges("abcd", &["ab".to_string(), "bc".to_string()]);
        assert_eq!(merged, vec![(0, 3)]);
    }

    #[test]
    fn snippet_adds_ellipsis_when_cut_at_the_front() {
        let text = format!("{} needle {}", "x".repeat(200), "y".repeat(200));
        let s = snippet(&text, "needle", 60);
        assert!(s.starts_with('…') && s.contains("needle"));
        assert_eq!(snippet("short needle", "needle", 60), "short needle");
    }

    #[test]
    fn handles_umlauts_without_shifting_positions() {
        let r = match_ranges("Größe ÄRGER Ärger", &["ärger".to_string()]);
        assert_eq!(r, vec![(6, 11), (12, 17)]);
    }

    #[test]
    fn session_ids_are_validated() {
        assert!(valid_sid("00190b24-a6fc-4771-b593-bda449e268e5"));
        assert!(!valid_sid("../../etc/passwd"));
        assert!(!valid_sid("00190b24-a6fc-4771-b593-bda449e268eZ"));
    }

    #[test]
    fn resume_command_template_is_filled_and_split() {
        let c = Config { resume_command: "cl -r {id}".into(), extra_hosts: vec![] };
        assert_eq!(resume_words(&c, "abc"), vec!["cl", "-r", "abc"]);
    }

    #[test]
    fn age_text_uses_the_right_unit() {
        std::env::set_var("HERDR_CHAT_SEARCH_LANG", "en");
        assert_eq!(fmt_age(30.0), "1 min");
        assert_eq!(fmt_age(7200.0), "2 h");
        assert_eq!(fmt_age(3.0 * 86400.0), "3 d");
    }
}
