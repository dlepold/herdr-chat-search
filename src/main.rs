// herdr-chat-search: entry point. Created 2026-11-08.
//
//   herdr-chat-search            terminal UI (same as `ui`), meant as a Herdr popup
//   herdr-chat-search ui         terminal UI
//   herdr-chat-search serve      local browser panel (default http://127.0.0.1:5132)
//   herdr-chat-search index      build or refresh the search index
//   herdr-chat-search list       print search results as plain text (scripting)

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
        "-h" | "--help" | "help" => usage(),
        _ => {
            usage();
            std::process::exit(2);
        }
    }
}
