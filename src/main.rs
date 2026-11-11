// herdr-chat-search: entry point. Created 2026-11-08.
//
//   herdr-chat-search            terminal UI (same as `ui`), meant as a Herdr popup
//   herdr-chat-search ui         terminal UI
//   herdr-chat-search serve      local browser panel (default http://127.0.0.1:5132)
//   herdr-chat-search index      build or refresh the search index
//   herdr-chat-search list       print search results as plain text (scripting)
//   herdr-chat-search selftest   index and search a tiny fake session (used by the release build)

mod core;
mod panel;
mod tui;

use core::t;

fn usage() {
    println!(
        "herdr-chat-search {}\n\n\
         USAGE\n  herdr-chat-search [ui]                    {}\n  \
         herdr-chat-search serve [--port N]        {}\n  \
         herdr-chat-search index [--age DAYS]      {}\n  \
         herdr-chat-search list [--age DAYS] [WORDS…]   {}\n",
        env!("CARGO_PKG_VERSION"),
        t("Terminal-Oberfläche (Popup in Herdr)", "terminal UI (popup inside Herdr)"),
        t("Browser-Panel auf localhost", "local browser panel"),
        t("Suchindex aufbauen/aktualisieren", "build/refresh the search index"),
        t("Treffer als Text ausgeben", "print hits as plain text"),
    );
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

fn run_ui() {
    let cfg = core::load_config();
    match tui::run() {
        Ok(Some(s)) => {
            match core::open_in_herdr(&s, &cfg) {
                Ok(log) => {
                    for l in log {
                        println!("→ {l}");
                    }
                }
                Err(e) => {
                    eprintln!("{e}");
                    eprintln!("{}", t("Enter zum Schließen", "Press Enter to close"));
                    let mut s = String::new();
                    let _ = std::io::stdin().read_line(&mut s);
                }
            }
        }
        Ok(None) => {}
        Err(e) => eprintln!("herdr-chat-search: {e}"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("ui");
    match cmd {
        "ui" => run_ui(),
        "serve" => {
            let port = arg_value(&args, "--port").and_then(|p| p.parse().ok()).unwrap_or(5132);
            panel::serve(port);
        }
        "index" => {
            let age: u32 = arg_value(&args, "--age").and_then(|p| p.parse().ok()).unwrap_or(90);
            let t0 = std::time::Instant::now();
            let files = core::scan_files();
            let live = core::live_sessions(&files);
            let exclude = live.iter().map(|s| s.sid.clone()).collect();
            let mut paths: Vec<_> = live.iter().filter_map(|s| s.file.clone()).collect();
            paths.extend(core::old_sessions(&files, age, &exclude).into_iter().filter_map(|s| s.file));
            println!("→ {} {} ({} {age} {})", t("Index für", "indexing"), paths.len(), t("Sitzungen, beendete der letzten", "sessions, closed ones of the last"), t("Tage", "days"));
            core::ensure_index(&paths);
            println!("✅ {:.1} s, {:.0} MB  ({})", t0.elapsed().as_secs_f32(), core::index_size_mb(), core::cache_dir().display());
        }
        "list" => {
            let age: u32 = arg_value(&args, "--age").and_then(|p| p.parse().ok()).unwrap_or(0);
            let mut words = vec![];
            let mut it = args.iter().skip(1);
            while let Some(a) = it.next() {
                if a == "--age" {
                    it.next();
                } else {
                    words.push(a.clone());
                }
            }
            for r in core::search(&words.join(" "), age) {
                let st = if r.s.live { r.s.status.clone() } else { t("beendet", "closed").to_string() };
                let sn = if r.text.is_empty() { String::new() } else { core::snippet(&r.text, &words.first().cloned().unwrap_or_default(), 100) };
                println!("{}\t{}\t{}\t{}\t{}\t{}", r.s.sid, st, r.s.title, r.s.ws, r.n, sn);
            }
        }
        "-V" | "--version" | "version" => println!("herdr-chat-search {}", env!("CARGO_PKG_VERSION")),
        "selftest" => match selftest() {
            Ok(()) => println!("selftest ok"),
            Err(e) => {
                eprintln!("selftest failed: {e}");
                std::process::exit(1);
            }
        },
        "-h" | "--help" | "help" => usage(),
        _ => {
            usage();
            std::process::exit(2);
        }
    }
}

/// Builds a fake Claude session in a temp folder, then indexes and searches it. The release
/// workflow runs this on every target CPU, so a binary that cannot do its core job never ships.
fn selftest() -> Result<(), String> {
    use std::fs;
    let dir = std::env::temp_dir().join(format!("herdr-chat-search-selftest-{}", std::process::id()));
    let proj = dir.join("claude/projects/-tmp-demo");
    fs::create_dir_all(&proj).map_err(|e| e.to_string())?;
    let sid = "11111111-2222-3333-4444-555555555555";
    let lines = [
        r#"{"parentUuid":null,"type":"user","message":{"role":"user","content":"Hello <system-reminder>hidden noise</system-reminder> Marzipan question"},"timestamp":"2026-01-01T10:00:00.000Z","cwd":"/tmp/demo"}"#,
        r#"{"parentUuid":"a","message":{"role":"assistant","content":[{"type":"text","text":"An answer about marzipan"},{"type":"tool_use","name":"Bash"}]},"type":"assistant","timestamp":"2026-01-01T10:00:05.000Z"}"#,
        r#"{"type":"ai-title","aiTitle":"Demo title","sessionId":"11111111-2222-3333-4444-555555555555"}"#,
    ];
    fs::write(proj.join(format!("{sid}.jsonl")), lines.join("\n") + "\n").map_err(|e| e.to_string())?;
    std::env::set_var("CLAUDE_CONFIG_DIR", dir.join("claude"));
    std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
    std::env::set_var("HERDR_BIN_PATH", "/nonexistent/herdr");
    let result = (|| {
        let hit = core::search("MARZIPAN question", 7);
        if hit.len() != 1 || hit[0].n != 1 || hit[0].s.title != "Demo title" || hit[0].s.cwd != "/tmp/demo" {
            return Err(format!("expected 1 session with 1 hit, got {} rows", hit.len()));
        }
        let both = core::search("marzipan", 7);
        if both.len() != 1 || both[0].n != 2 {
            return Err("expected 2 matching messages for 'marzipan'".into());
        }
        if !core::search("hidden noise", 7).is_empty() {
            return Err("system-reminder text must not be searchable".into());
        }
        Ok(())
    })();
    let _ = fs::remove_dir_all(&dir);
    result
}
