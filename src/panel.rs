// herdr-chat-search: local browser panel. Created 2026-11-08.
// A tiny HTTP server on 127.0.0.1 that serves one HTML page plus a JSON API over the same
// search core the terminal UI uses. Typical use: behind a Caddy name such as
// http://chats.localhost, started as a systemd user service.
//
// Security: the page shows private chat content and can start commands, and any web page in
// your browser can talk to 127.0.0.1. So every request must carry an allowed Host header
// (blocks DNS rebinding) and, if present, an allowed Origin header (blocks cross-site
// requests). POST requests must also be JSON. Session ids are validated before use.

use crate::core::{self, Config, Preview, Row, Session};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;
use tiny_http::{Header, Method, Request, Response, Server};

const PAGE: &str = include_str!("panel.html");
/// Host names accepted by default. More can be added with `extra_hosts` in the plugin config.
const DEFAULT_HOSTS: [&str; 3] = ["127.0.0.1", "localhost", "chats.localhost"];

fn host_allowed(name: &str, cfg: &Config) -> bool {
    DEFAULT_HOSTS.contains(&name) || cfg.extra_hosts.iter().any(|h| h == name)
}

pub fn serve(port: u16) {
    let server = match Server::http(("127.0.0.1", port)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("herdr-chat-search: cannot listen on 127.0.0.1:{port}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("herdr-chat-search panel: http://127.0.0.1:{port}/");
    thread::spawn(|| loop {
        index_all();
        thread::sleep(Duration::from_secs(60));
    });
    let cfg = core::load_config();
    for req in server.incoming_requests() {
        let cfg = cfg.clone();
        thread::spawn(move || handle(req, &cfg));
    }
}

/// Keeps the index of open agents and of the last 90 days fresh, so searches stay instant.
fn index_all() {
    let files = core::scan_files();
    let live = core::live_sessions(&files);
    let exclude = live.iter().map(|s| s.sid.clone()).collect();
    let mut paths: Vec<_> = live.iter().filter_map(|s| s.file.clone()).collect();
    paths.extend(core::old_sessions(&files, 90, &exclude).into_iter().filter_map(|s| s.file));
    core::ensure_index(&paths);
}

fn host_part(h: &str) -> String {
    let h = h.trim().trim_start_matches("http://").trim_start_matches("https://");
    let h = h.split('/').next().unwrap_or("");
    h.rsplit_once(':').map(|(a, _)| a).unwrap_or(h).to_lowercase()
}

fn header_of(req: &Request, name: &str) -> Option<String> {
    req.headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str().to_string())
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    out.push(v);
                    i += 2;
                } else {
                    out.push(b'%');
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (percent_decode(k), percent_decode(v)))
        .collect()
}

fn json_response(code: u16, v: Value) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut r = Response::from_string(v.to_string()).with_status_code(code);
    for (k, val) in [("Content-Type", "application/json; charset=utf-8"), ("Cache-Control", "no-store")] {
        r.add_header(Header::from_bytes(k, val).unwrap());
    }
    r
}

fn row_json(r: &Row, first_word: &str) -> Value {
    let key = if r.s.sid.is_empty() { format!("pane:{}", r.s.pane) } else { r.s.sid.clone() };
    json!({
        "key": key, "live": r.s.live, "agent": r.s.agent, "status": r.s.status,
        "title": r.s.title, "ws": r.s.ws, "cwd": r.s.cwd, "n": r.n,
        "age": core::fmt_age(core::now() - r.s.mtime),
        "text": if r.text.is_empty() { String::new() } else { core::snippet(&r.text, first_word, 170) },
        "hasLog": r.s.file.is_some(),
    })
}

fn find_session(key: &str) -> Option<Session> {
    if let Some(pane) = key.strip_prefix("pane:") {
        let files = core::scan_files();
        let mut s = core::live_cached(&files).into_iter().find(|s| s.pane == pane)?;
        core::fill_meta(&mut s);
        return Some(s);
    }
    if !core::valid_sid(key) {
        return None;
    }
    core::session_by_sid(key)
}

fn open_in_terminal(s: &Session, cfg: &Config) -> Result<Vec<String>, String> {
    if s.live {
        return Err(core::t("Die Sitzung läuft bereits in Herdr: dort springen statt neu öffnen.", "This session is already running in Herdr: jump to it instead.").into());
    }
    let script = format!("{}; exec bash", cfg.resume_command.replace("{id}", &s.sid));
    let cwd = if !s.cwd.is_empty() && Path::new(&s.cwd).is_dir() { s.cwd.clone() } else { std::env::var("HOME").unwrap_or_else(|_| "/".into()) };
    let (mut c, log) = if Path::new("/usr/bin/tilix").exists() {
        let mut c = Command::new("tilix");
        c.args(["-w", &cwd, "-e", &format!("bash -lc \"{script}\"")]);
        (c, format!("tilix -w {cwd} -e 'bash -lc \"{script}\"'"))
    } else {
        let mut c = Command::new("gnome-terminal");
        c.args([&format!("--working-directory={cwd}"), "--", "bash", "-lc", &script]);
        (c, format!("gnome-terminal --working-directory={cwd} -- bash -lc '{script}'"))
    };
    c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    c.spawn().map_err(|e| e.to_string())?;
    Ok(vec![log])
}

fn handle(mut req: Request, cfg: &Config) {
    let host_ok = header_of(&req, "Host").map(|h| host_allowed(&host_part(&h), cfg)).unwrap_or(false);
    let origin_ok = header_of(&req, "Origin").map(|o| host_allowed(&host_part(&o), cfg)).unwrap_or(true);
    if !host_ok || !origin_ok {
        let _ = req.respond(json_response(403, json!({"error": "forbidden host or origin"})));
        return;
    }
    let url = req.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let q = parse_query(query);
    let method = req.method().clone();
    let resp = match (method, path) {
        (Method::Get, "/") => {
            let mut r = Response::from_string(PAGE);
            for (k, v) in [
                ("Content-Type", "text/html; charset=utf-8"),
                ("Cache-Control", "no-store"),
                ("X-Content-Type-Options", "nosniff"),
                ("Referrer-Policy", "no-referrer"),
                ("Content-Security-Policy", "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; img-src data:"),
            ] {
                r.add_header(Header::from_bytes(k, v).unwrap());
            }
            let _ = req.respond(r);
            return;
        }
        (Method::Get, "/api/config") => json_response(200, json!({
            "resumeCommand": cfg.resume_command,
            "ages": core::AGE_DAYS,
        })),
        (Method::Get, "/api/search") => {
            let text = q.get("q").cloned().unwrap_or_default();
            let age: u32 = q.get("age").and_then(|a| a.parse().ok()).filter(|a| core::AGE_DAYS.contains(a)).unwrap_or(0);
            let t0 = std::time::Instant::now();
            let words = core::split_words(&text);
            let first = words.first().cloned().unwrap_or_default();
            let rows: Vec<Value> = core::search(&text, age).iter().map(|r| row_json(r, &first)).collect();
            json_response(200, json!({"rows": rows, "ms": t0.elapsed().as_millis() as u64}))
        }
        (Method::Get, "/api/preview") => {
            let key = q.get("id").cloned().unwrap_or_default();
            let words = core::split_words(q.get("q").map(|s| s.as_str()).unwrap_or(""));
            match find_session(&key) {
                None => json_response(404, json!({"error": "unknown session"})),
                Some(s) => {
                    let conv = |hits: Vec<core::Hit>| -> Vec<Value> {
                        hits.into_iter()
                            .map(|h| {
                                let text = match words.first() {
                                    Some(w) => core::snippet(&h.text, w, 420),
                                    None => h.text.chars().take(420).collect(),
                                };
                                json!({"ts": core::fmt_ts(&h.ts), "role": h.role, "text": text})
                            })
                            .collect()
                    };
                    let v = match core::preview(&s.sid, s.file.is_some(), &words) {
                        Preview::NoLog => json!({"kind": "nolog"}),
                        Preview::Hits { n, hits } => json!({"kind": "hits", "n": n, "hits": conv(hits)}),
                        Preview::Recent(hits) => json!({"kind": "recent", "hits": conv(hits)}),
                    };
                    json_response(200, v)
                }
            }
        }
        (Method::Post, "/api/open") => {
            let is_json = header_of(&req, "Content-Type").map(|c| c.starts_with("application/json")).unwrap_or(false);
            let mut body = String::new();
            if is_json {
                use std::io::Read;
                let _ = req.as_reader().take(4096).read_to_string(&mut body);
            }
            let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let key = v.get("id").and_then(|x| x.as_str()).unwrap_or("");
            let mode = v.get("mode").and_then(|x| x.as_str()).unwrap_or("herdr");
            match (is_json, find_session(key)) {
                (false, _) => json_response(415, json!({"ok": false, "error": "JSON required"})),
                (_, None) => json_response(404, json!({"ok": false, "error": "unknown session"})),
                (_, Some(s)) => {
                    let r = match mode {
                        "herdr" => core::open_in_herdr(&s, cfg),
                        "terminal" => open_in_terminal(&s, cfg),
                        _ => Err("unknown mode".into()),
                    };
                    match r {
                        Ok(log) => json_response(200, json!({"ok": true, "log": log})),
                        Err(e) => json_response(500, json!({"ok": false, "error": e})),
                    }
                }
            }
        }
        _ => json_response(404, json!({"error": "not found"})),
    };
    let _ = req.respond(resp);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(extra: &[&str]) -> Config {
        Config { resume_command: String::new(), extra_hosts: extra.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn host_part_strips_scheme_port_and_case() {
        assert_eq!(host_part("Chats.Localhost:5132"), "chats.localhost");
        assert_eq!(host_part("http://127.0.0.1:5132/x"), "127.0.0.1");
    }

    #[test]
    fn only_known_hosts_pass() {
        assert!(host_allowed("localhost", &cfg(&[])));
        assert!(!host_allowed("evil.example.com", &cfg(&[])));
        assert!(host_allowed("chats.example", &cfg(&["chats.example"])));
        // A look-alike must not match.
        assert!(!host_allowed("chats.localhost.evil.com", &cfg(&[])));
    }

    #[test]
    fn query_strings_are_decoded() {
        let q = parse_query("q=gr%C3%B6%C3%9Fe+test&age=7&bad=%zz");
        assert_eq!(q["q"], "größe test");
        assert_eq!(q["age"], "7");
        assert_eq!(q["bad"], "%zz");
    }
}
