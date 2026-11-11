// herdr-chat-search: terminal UI (popup inside Herdr). Created 2026-11-08.
// Left: hits per agent/session. Right: preview of the newest matching messages.
// The search runs in a worker thread; every keystroke only sends a request, and the worker
// answers the newest one, so typing never blocks.

use crate::core::{self, t, Preview, Row, Session, AGE_DAYS};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::io;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

struct Resp {
    seq: u64,
    rows: Vec<Row>,
    ms: u128,
}

struct App {
    q: String,
    age_idx: usize,
    rows: Vec<Row>,
    state: ListState,
    preview: Vec<Line<'static>>,
    pscroll: u16,
    sent: u64,
    done: u64,
    ms: u128,
    tx: Sender<(u64, String, u32)>,
}

impl App {
    fn request(&mut self) {
        self.sent += 1;
        let _ = self.tx.send((self.sent, self.q.clone(), AGE_DAYS[self.age_idx]));
    }

    fn selected(&self) -> Option<&Row> {
        self.state.selected().and_then(|i| self.rows.get(i))
    }

    fn refresh_preview(&mut self) {
        self.pscroll = 0;
        let words = core::split_words(&self.q);
        self.preview = match self.selected() {
            Some(r) => build_preview(r, &words),
            None => vec![],
        };
    }

    fn select_delta(&mut self, d: isize) {
        if self.rows.is_empty() {
            return;
        }
        let cur = self.state.selected().unwrap_or(0) as isize;
        let n = (cur + d).clamp(0, self.rows.len() as isize - 1) as usize;
        self.state.select(Some(n));
        self.refresh_preview();
    }
}

fn hl_spans(text: &str, words: &[String], base: Style) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = vec![];
    let mut pos = 0;
    for (a, b) in core::match_ranges(text, words) {
        let (a, b) = (a.min(chars.len()), b.min(chars.len()));
        if a > pos {
            out.push(Span::styled(chars[pos..a].iter().collect::<String>(), base));
        }
        out.push(Span::styled(
            chars[a..b].iter().collect::<String>(),
            base.fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ));
        pos = b;
    }
    if pos < chars.len() {
        out.push(Span::styled(chars[pos..].iter().collect::<String>(), base));
    }
    out
}

fn build_preview(r: &Row, words: &[String]) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let who = |role: &str| match role {
        "user" => t("Du ", "You"),
        "assistant" => "AI ",
        _ => "?  ",
    };
    let mut out = vec![];
    match core::preview(&r.s.sid, r.s.file.is_some(), words) {
        Preview::NoLog => {
            out.push(Line::from(t(
                "Kein Chat-Protokoll bekannt (z. B. Codex-Agent: Herdr meldet dort keine Session-ID). Gefunden wurde nur über Titel oder Workspace-Name.",
                "No chat log known (e.g. a Codex agent: Herdr reports no session id there). Matched by title or workspace name only.",
            )));
        }
        Preview::Hits { n, hits } => {
            out.push(Line::from(vec![
                Span::styled(format!("{n} {}", t("Treffer", "hits")), Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(format!("  ({} {})", t("die neuesten", "newest"), hits.len()), dim),
            ]));
            out.push(Line::from(""));
            for h in hits {
                let text = core::snippet(&h.text, &words[0], 320);
                let mut l = vec![Span::styled(format!("{} ", core::fmt_ts(&h.ts)), dim), Span::raw(format!("{} ", who(&h.role)))];
                l.extend(hl_spans(&text, words, Style::default()));
                out.push(Line::from(l));
                out.push(Line::from(""));
            }
        }
        Preview::Recent(hits) => {
            out.push(Line::from(vec![
                Span::styled(t("Letzte Nachrichten", "Latest messages"), Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(format!("  ({})", t("Stichwort eintippen für Treffer", "type a keyword for hits")), dim),
            ]));
            out.push(Line::from(""));
            for h in hits {
                let text: String = h.text.chars().take(300).collect();
                out.push(Line::from(vec![
                    Span::styled(format!("{} ", core::fmt_ts(&h.ts)), dim),
                    Span::raw(format!("{} {}", who(&h.role), text)),
                ]));
                out.push(Line::from(""));
            }
        }
    }
    out
}

fn row_item(r: &Row, words: &[String]) -> ListItem<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    let (dot, state, col) = if r.s.live {
        let c = match r.s.status.as_str() {
            "working" => Color::Yellow,
            "blocked" => Color::Red,
            _ => Color::Green,
        };
        ("●", r.s.status.clone(), c)
    } else {
        ("○", t("beendet", "closed").to_string(), Color::DarkGray)
    };
    let pad = |s: &str, n: usize| {
        let c: String = s.chars().take(n).collect();
        format!("{:<n$}", c, n = n)
    };
    let cnt = if r.n > 0 { format!("{:>4} {}", r.n, t("Tr.", "hit")) } else { "   –   ".to_string() };
    let mut l = vec![
        Span::styled(format!("{dot} "), Style::default().fg(col)),
        Span::styled(pad(&state, 8), Style::default().fg(col)),
        Span::styled(pad(&r.s.title, 36), Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(format!(" [{}] {:>6}", pad(&r.s.ws, 18), core::fmt_age(core::now() - r.s.mtime)), dim),
        Span::styled(format!("  {cnt}  "), dim),
    ];
    if !r.text.is_empty() && !words.is_empty() {
        let sn = core::snippet(&r.text, &words[0], 110);
        l.extend(hl_spans(&sn, words, Style::default()));
    }
    ListItem::new(Line::from(l))
}

fn draw(f: &mut Frame, app: &mut App) {
    let [head, input, main] = Layout::vertical([Constraint::Length(2), Constraint::Length(1), Constraint::Min(1)]).areas(f.area());
    let dim = Style::default().fg(Color::DarkGray);
    let age = AGE_DAYS[app.age_idx];
    let scope = if age == 0 {
        t("nur offene Agenten", "open agents only").to_string()
    } else {
        format!("{} {} {}", t("offene + beendete Sitzungen der letzten", "open + closed sessions of the last"), age, t("Tage", "days"))
    };
    let mut keys: Vec<Span> = vec![];
    for (i, d) in AGE_DAYS.iter().enumerate() {
        let txt = format!("F{} {}", i + 1, core::age_label(*d));
        if i == app.age_idx {
            keys.push(Span::styled(format!("[{txt}]"), Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan)));
        } else {
            keys.push(Span::raw(txt));
        }
        keys.push(Span::raw("  "));
    }
    f.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(format!("{} {scope}  ", t("Chat-Suche:", "Chat search:")), Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(t("· Enter = springen/öffnen · Esc = zu · Bild↑↓ = Vorschau", "· Enter = jump/open · Esc = close · PgUp/PgDn = preview"), dim),
            ]),
            Line::from(keys),
        ]),
        head,
    );
    let busy = app.sent > app.done;
    let info = if busy { format!("  {}", t("suche …", "searching …")) } else { format!("  {} · {} ms", app.rows.len(), app.ms) };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(t("Suche > ", "Search > "), Style::default().fg(Color::Cyan)),
            Span::raw(app.q.clone()),
            Span::styled(info, dim),
        ])),
        input,
    );
    let prompt_w = t("Suche > ", "Search > ").chars().count() as u16;
    f.set_cursor_position((input.x + prompt_w + app.q.chars().count() as u16, input.y));

    let [left, right]: [Rect; 2] = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(main);
    let words = core::split_words(&app.q);
    let items: Vec<ListItem> = app.rows.iter().map(|r| row_item(r, &words)).collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::TOP | Borders::RIGHT))
        .highlight_style(Style::default().bg(Color::Rgb(40, 44, 60)))
        .highlight_symbol("▌");
    f.render_stateful_widget(list, left, &mut app.state);
    f.render_widget(
        Paragraph::new(app.preview.clone())
            .block(Block::default().borders(Borders::TOP))
            .wrap(Wrap { trim: false })
            .scroll((app.pscroll, 0)),
        right,
    );
}

fn app_loop(term: &mut DefaultTerminal, tx: Sender<(u64, String, u32)>, rx: Receiver<Resp>) -> io::Result<Option<Session>> {
    let mut app = App { q: String::new(), age_idx: 0, rows: vec![], state: ListState::default(), preview: vec![], pscroll: 0, sent: 0, done: 0, ms: 0, tx };
    app.request();
    loop {
        while let Ok(r) = rx.try_recv() {
            app.done = r.seq;
            if r.seq == app.sent {
                let keep = app.selected().map(|s| s.s.sid.clone());
                app.ms = r.ms;
                app.rows = r.rows;
                let idx = keep.and_then(|k| app.rows.iter().position(|x| !k.is_empty() && x.s.sid == k)).unwrap_or(0);
                app.state.select(if app.rows.is_empty() { None } else { Some(idx) });
                app.refresh_preview();
            }
        }
        term.draw(|f| draw(f, &mut app))?;
        if !event::poll(Duration::from_millis(40))? {
            continue;
        }
        let Event::Key(k) = event::read()? else { continue };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let mut changed = false;
        match k.code {
            KeyCode::Esc => return Ok(None),
            KeyCode::Char('c') if ctrl => return Ok(None),
            KeyCode::Enter => return Ok(app.selected().map(|r| r.s.clone())),
            KeyCode::Up => app.select_delta(-1),
            KeyCode::Down => app.select_delta(1),
            KeyCode::Char('p') if ctrl => app.select_delta(-1),
            KeyCode::Char('n') if ctrl => app.select_delta(1),
            KeyCode::PageUp => app.pscroll = app.pscroll.saturating_sub(5),
            KeyCode::PageDown => app.pscroll = app.pscroll.saturating_add(5),
            KeyCode::F(n) if (1..=7).contains(&n) => {
                app.age_idx = n as usize - 1;
                changed = true;
            }
            KeyCode::Char(c) if alt && ('0'..='6').contains(&c) => {
                app.age_idx = c as usize - '0' as usize;
                changed = true;
            }
            KeyCode::Char('u') if ctrl => {
                app.q.clear();
                changed = true;
            }
            KeyCode::Backspace => {
                changed = app.q.pop().is_some();
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                app.q.push(c);
                changed = true;
            }
            _ => {}
        }
        if changed {
            app.request();
        }
    }
}

pub fn run() -> io::Result<Option<Session>> {
    let (req_tx, req_rx) = mpsc::channel::<(u64, String, u32)>();
    let (res_tx, res_rx) = mpsc::channel::<Resp>();
    std::thread::spawn(move || {
        while let Ok(mut r) = req_rx.recv() {
            while let Ok(newer) = req_rx.try_recv() {
                r = newer; // only the newest request matters
            }
            let t0 = Instant::now();
            let rows = core::search(&r.1, r.2);
            if res_tx.send(Resp { seq: r.0, rows, ms: t0.elapsed().as_millis() }).is_err() {
                break;
            }
        }
    });
    let mut term = ratatui::init();
    let res = app_loop(&mut term, req_tx, res_rx);
    ratatui::restore();
    res
}
