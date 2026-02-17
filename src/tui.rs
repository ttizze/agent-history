use crate::{
    args::Args,
    indexer::{IndexerEvent, MessageRecord, Role, SourceKind},
    search,
};
use anyhow::Context as _;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Text},
    widgets::{Block, Borders, Gauge, Paragraph, Wrap},
};
use std::{
    cmp,
    collections::HashMap,
    env, fs,
    io::{self, Stdout, Write as _},
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc,
    time::Duration,
    time::Instant,
};

#[derive(Debug, Default, Clone)]
struct IndexingProgress {
    scanning_roots: bool,
    scanned_dirs: usize,
    found_roots: usize,
    found_files: usize,
    scan_current: Option<PathBuf>,

    total_files: usize,
    processed_files: usize,
    records: usize,
    sessions: usize,
    current: Option<PathBuf>,
    last_warn: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionSummary {
    pub(crate) source: SourceKind,
    pub(crate) session_id: String,
    pub(crate) first_user_idx: usize,
    pub(crate) last_ts: Option<String>,
    pub(crate) dir: String,
    pub(crate) first_line: String,
}

#[derive(Debug, Default)]
struct SessionAgg<'a> {
    record_indices: Vec<usize>,
    last_ts: Option<&'a str>,
    cwd: Option<&'a str>,
}

pub(crate) fn build_session_index(all: &[MessageRecord]) -> (Vec<SessionSummary>, Vec<Vec<usize>>) {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    struct SessionKeyRef<'a> {
        source: SourceKind,
        session_id: &'a str,
    }

    let mut by_session: HashMap<SessionKeyRef<'_>, SessionAgg<'_>> = HashMap::new();

    fn pick_first_idx(
        all: &[MessageRecord],
        indices: &[usize],
        mut pred: impl FnMut(&MessageRecord) -> bool,
    ) -> Option<usize> {
        let mut best_idx: Option<usize> = None;
        let mut best_ts: Option<&str> = None;

        for &idx in indices {
            let rec = &all[idx];
            if !pred(rec) {
                continue;
            }

            let ts = rec.timestamp.as_deref();
            let is_earlier = match (ts, best_ts) {
                (Some(ts), Some(cur)) => ts_cmp_str(ts, cur) == cmp::Ordering::Less,
                (Some(_), None) => true,
                (None, None) => best_idx.is_none(),
                (None, Some(_)) => false,
            };

            if best_idx.is_none() || is_earlier {
                best_idx = Some(idx);
                best_ts = ts;
            }
        }

        best_idx
    }

    for (idx, rec) in all.iter().enumerate() {
        let Some(session_id) = rec.session_id.as_deref() else {
            continue;
        };

        let entry = by_session
            .entry(SessionKeyRef {
                source: rec.source,
                session_id,
            })
            .or_default();

        entry.record_indices.push(idx);

        if let Some(ts) = rec.timestamp.as_deref()
            && entry
                .last_ts
                .is_none_or(|cur| ts_cmp_str(ts, cur) == cmp::Ordering::Greater)
        {
            entry.last_ts = Some(ts);
        }

        if entry.cwd.is_none_or(|s| s.trim().is_empty())
            && let Some(cwd) = rec.cwd.as_deref()
            && !cwd.trim().is_empty()
        {
            entry.cwd = Some(cwd);
        }
    }

    let mut items: Vec<(SessionSummary, Vec<usize>)> = Vec::new();

    for (key, agg) in by_session {
        let first_user_idx = pick_first_idx(all, &agg.record_indices, |rec| match key.source {
            SourceKind::CodexHistoryJsonl => true,
            _ => rec.role == Role::User && !is_noise_user_message(&rec.text),
        });
        let Some(first_user_idx) = first_user_idx else {
            continue;
        };

        items.push((
            SessionSummary {
                source: key.source,
                session_id: key.session_id.to_string(),
                first_user_idx,
                last_ts: agg.last_ts.map(|s| s.to_string()),
                dir: dir_name_from_cwd(
                    agg.cwd
                        .or_else(|| all[first_user_idx].cwd.as_deref())
                        .unwrap_or(""),
                ),
                first_line: {
                    let rec = &all[first_user_idx];
                    let s = if rec.source == SourceKind::CodexSessionJsonl
                        && looks_like_codex_title_task_prompt(&rec.text)
                    {
                        extract_user_prompt_line_from_codex_title_task(&rec.text)
                            .unwrap_or_else(|| first_non_empty_line(&rec.text).to_string())
                    } else {
                        first_non_empty_line(&rec.text).to_string()
                    };
                    s.replace(['\t', '\n'], " ")
                },
            },
            agg.record_indices,
        ));
    }

    items.sort_by(|(a, _), (b, _)| {
        ts_cmp_opt(a.last_ts.as_deref(), b.last_ts.as_deref())
            .reverse()
            .then_with(|| a.session_id.cmp(&b.session_id))
            .then_with(|| source_sort_key(a.source).cmp(&source_sort_key(b.source)))
    });

    let (sessions, records): (Vec<_>, Vec<_>) = items.into_iter().unzip();
    (sessions, records)
}

fn source_sort_key(source: SourceKind) -> u8 {
    match source {
        SourceKind::CodexSessionJsonl => 0,
        SourceKind::CodexHistoryJsonl => 1,
        SourceKind::ClaudeProjectJsonl => 2,
    }
}

fn provider_icon(source: SourceKind) -> &'static str {
    match source {
        SourceKind::ClaudeProjectJsonl => "C",
        SourceKind::CodexSessionJsonl | SourceKind::CodexHistoryJsonl => "O",
    }
}

fn short_ts(ts: Option<&str>) -> &str {
    let ts = ts.unwrap_or("");
    ts.get(0..19).unwrap_or(ts)
}

fn first_non_empty_line(s: &str) -> &str {
    for line in s.lines() {
        let line = line.trim();
        if !line.is_empty() {
            return line;
        }
    }
    ""
}

fn looks_like_codex_title_task_prompt(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("You are a helpful assistant. You will be presented with a user prompt")
        && t.contains("\nUser prompt:")
}

fn extract_user_prompt_line_from_codex_title_task(text: &str) -> Option<String> {
    let (_, tail) = text.split_once("User prompt:")?;
    let mut last: Option<&str> = None;
    for line in tail.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("images:")
            || lower.starts_with("local_images:")
            || lower.starts_with("text_elements:")
        {
            continue;
        }
        last = Some(line);
    }
    last.map(|s| s.to_string())
}

fn dir_name_from_cwd(cwd: &str) -> String {
    let s = cwd.trim();
    let s = s.trim_end_matches('/');
    if s.is_empty() {
        return "-".to_string();
    }
    Path::new(s)
        .file_name()
        .and_then(|x| x.to_str())
        .filter(|x| !x.trim().is_empty())
        .unwrap_or(s)
        .to_string()
}

fn is_agents_instructions(text: &str) -> bool {
    let t = text.trim_start();
    let first = t.lines().next().unwrap_or("").trim_start();
    let lower = first.to_ascii_lowercase();
    lower.starts_with("# agents.md instructions for ")
        || lower.starts_with("agents.md instructions for ")
}

fn is_environment_context(text: &str) -> bool {
    let t = text.trim_start();
    let first = t.lines().next().unwrap_or("").trim_start();
    let lower = first.to_ascii_lowercase();
    lower.starts_with("<environment_context")
}

fn is_noise_user_message(text: &str) -> bool {
    is_agents_instructions(text) || is_environment_context(text)
}

fn ts_cmp_opt(a: Option<&str>, b: Option<&str>) -> cmp::Ordering {
    match (a, b) {
        (None, None) => cmp::Ordering::Equal,
        (None, Some(_)) => cmp::Ordering::Less,
        (Some(_), None) => cmp::Ordering::Greater,
        (Some(a), Some(b)) => ts_cmp_str(a, b),
    }
}

fn ts_cmp_str(a: &str, b: &str) -> cmp::Ordering {
    match (parse_epoch(a), parse_epoch(b)) {
        (Some(a), Some(b)) => a.cmp(&b),
        _ => a.cmp(b),
    }
}

fn parse_epoch(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

#[derive(Debug)]
struct App {
    query: String,
    max_results: usize,

    all: Vec<MessageRecord>,
    sessions: Vec<SessionSummary>,
    session_records: Vec<Vec<usize>>,
    filtered: Vec<usize>,
    selected: usize,
    offset: usize,

    last_query: String,
    last_results: Vec<usize>,

    indexing: IndexingProgress,
    ready: bool,

    spinner: usize,
}

pub fn run(args: Args) -> anyhow::Result<()> {
    let rx = crate::indexer::spawn_indexer_from_args(args.clone());

    let mut stdout = io::stdout();
    enable_raw_mode().context("raw modeの有効化に失敗")?;
    execute!(stdout, EnterAlternateScreen, cursor::Hide).context("画面切替に失敗")?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("Terminal初期化に失敗")?;
    terminal.clear().ok();

    let res = run_app(&mut terminal, rx, args);

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen, cursor::Show).ok();
    terminal.show_cursor().ok();

    res
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    rx: mpsc::Receiver<IndexerEvent>,
    args: Args,
) -> anyhow::Result<()> {
    let mut app = App {
        query: args.query.unwrap_or_default(),
        max_results: args.max_results,
        all: Vec::new(),
        sessions: Vec::new(),
        session_records: Vec::new(),
        filtered: Vec::new(),
        selected: 0,
        offset: 0,
        last_query: String::new(),
        last_results: Vec::new(),
        indexing: IndexingProgress {
            // root 探索が重いので、最初から「探索中」を出しておく
            scanning_roots: !args.no_default_roots,
            ..IndexingProgress::default()
        },
        ready: false,
        spinner: 0,
    };

    loop {
        while let Ok(ev) = rx.try_recv() {
            handle_indexer_event(&mut app, ev);
        }

        terminal.draw(|f| ui(f, &mut app)).context("描画に失敗")?;

        if !event::poll(Duration::from_millis(50)).context("event pollに失敗")? {
            continue;
        }

        let ev = event::read().context("event readに失敗")?;
        let Event::Key(key) = ev else {
            continue;
        };

        if handle_key(terminal, &mut app, key)? {
            break;
        }
    }

    Ok(())
}

fn handle_indexer_event(app: &mut App, ev: IndexerEvent) {
    match ev {
        IndexerEvent::RootScanProgress {
            scanned_dirs,
            found_roots,
            found_files,
            current,
        } => {
            app.indexing.scanning_roots = true;
            app.indexing.scanned_dirs = scanned_dirs;
            app.indexing.found_roots = found_roots;
            app.indexing.found_files = found_files;
            app.indexing.scan_current = Some(current);
        }
        IndexerEvent::Discovered { total_files } => {
            app.indexing.scanning_roots = false;
            app.indexing.total_files = total_files;
        }
        IndexerEvent::Progress {
            processed_files,
            total_files,
            records,
            sessions,
            current,
        } => {
            app.indexing.scanning_roots = false;
            app.indexing.processed_files = processed_files;
            app.indexing.total_files = total_files;
            app.indexing.records = records;
            app.indexing.sessions = sessions;
            app.indexing.current = Some(current);
        }
        IndexerEvent::Warn { message } => {
            app.indexing.last_warn = Some(message);
        }
        IndexerEvent::Done { records } => {
            app.all = records;
            let (sessions, session_records) = build_session_index(&app.all);
            app.sessions = sessions;
            app.session_records = session_records;
            app.ready = true;
            app.update_results();
        }
    }
}

fn handle_key(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    key: KeyEvent,
) -> anyhow::Result<bool> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Ok(true);
    }

    if key.code == KeyCode::Esc {
        return Ok(true);
    }

    // インデックス作成中でもクエリ入力だけは先に受け付ける
    match key.code {
        KeyCode::Backspace => {
            app.query.pop();
            app.update_results();
            return Ok(false);
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.query.clear();
            app.update_results();
            return Ok(false);
        }
        KeyCode::Char(c) => {
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT)
            {
                app.query.push(c);
                app.update_results();
                return Ok(false);
            }
        }
        _ => {}
    }

    if !app.ready {
        return Ok(false);
    }

    match key.code {
        KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(rec) = app.selected_record() {
                open_in_pager(terminal, rec)?;
            }
        }
        KeyCode::Enter => {
            if let Some(rec) = app.selected_record() {
                match resume_target_for_record(app, rec) {
                    Some(target) => {
                        let sid = rec.session_id.as_deref().unwrap_or("");
                        let status = run_with_tui_suspended(terminal, || {
                            let mut cmd = Command::new(&target.program);
                            cmd.args(&target.args);
                            if let Some(cwd) = target.current_dir.as_ref() {
                                cmd.current_dir(cwd);
                            }

                            for line in resume_loading_lines(&target, sid) {
                                eprintln!("{line}");
                            }

                            // `status()` でブロックすると何も出せないので、`spawn()`して短時間だけ
                            // "動いてる" ローディング表示を出す（その間に外部CLIが起動する想定）。
                            let mut child = cmd.spawn().context("プロセス起動に失敗しました")?;
                            if let Some(st) =
                                animate_resume_loader(&target.program, sid, &mut child)?
                            {
                                return Ok(st);
                            }
                            child.wait().context("プロセス待機に失敗しました")
                        });

                        match status {
                            Ok(st) if st.success() => {
                                app.indexing.last_warn = None;
                            }
                            Ok(st) => {
                                app.indexing.last_warn = Some(format!(
                                    "resume失敗: {} (code={})",
                                    target.program,
                                    st.code()
                                        .map(|c| c.to_string())
                                        .unwrap_or_else(|| "?".to_string())
                                ));
                            }
                            Err(e) => {
                                app.indexing.last_warn =
                                    Some(format!("resume失敗: {}: {e}", target.program));
                            }
                        }
                    }
                    None => {
                        app.indexing.last_warn = Some("この行はresume対象にできません".to_string());
                    }
                }
            }
        }
        KeyCode::Up => app.move_selection(-1),
        KeyCode::Down => app.move_selection(1),
        KeyCode::PageUp => app.page(-1),
        KeyCode::PageDown => app.page(1),
        KeyCode::Home => app.select_first(),
        KeyCode::End => app.select_last(),
        _ => {}
    }

    Ok(false)
}

impl App {
    fn update_results(&mut self) {
        let q = self.query.trim().to_string();

        let prev_selected_global = self.filtered.get(self.selected).copied();

        let max = self.max_results;
        let limit = |n: usize| -> bool { max != 0 && n >= max };

        let mut results: Vec<usize> = Vec::new();

        if q.is_empty() {
            let mut i = 0usize;
            while i < self.sessions.len() && !limit(results.len()) {
                results.push(i);
                i += 1;
            }
        } else {
            let base: Box<dyn Iterator<Item = usize>> = if !self.last_query.is_empty()
                && q.starts_with(&self.last_query)
                && !self.last_results.is_empty()
            {
                Box::new(self.last_results.iter().copied())
            } else {
                Box::new(0..self.sessions.len())
            };

            let compiled = search::CompiledQuery::new(&q);
            for idx in base {
                if self.session_matches(idx, &compiled) {
                    results.push(idx);
                    if limit(results.len()) {
                        break;
                    }
                }
            }
        }

        self.filtered = results;
        self.last_query = q;
        self.last_results = self.filtered.clone();

        self.offset = 0;
        self.selected = 0;
        if let Some(prev) = prev_selected_global
            && let Some(pos) = self.filtered.iter().position(|&x| x == prev)
        {
            self.selected = pos;
        }
    }

    fn session_matches(&self, session_idx: usize, query: &search::CompiledQuery) -> bool {
        let Some(record_idxs) = self.session_records.get(session_idx) else {
            return false;
        };
        for &idx in record_idxs {
            if query.matches_record(&self.all[idx]) {
                return true;
            }
        }
        false
    }

    fn selected_session(&self) -> Option<&SessionSummary> {
        let idx = *self.filtered.get(self.selected)?;
        self.sessions.get(idx)
    }

    fn selected_record(&self) -> Option<&MessageRecord> {
        let session = self.selected_session()?;
        self.all.get(session.first_user_idx)
    }

    fn move_selection(&mut self, delta: i32) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }
        let max = (self.filtered.len() - 1) as i32;
        let cur = self.selected as i32;
        let next = cmp::min(max, cmp::max(0, cur + delta));
        self.selected = next as usize;
    }

    fn page(&mut self, dir: i32) {
        if self.filtered.is_empty() {
            return;
        }
        let delta = 10i32 * dir;
        self.move_selection(delta);
    }

    fn select_first(&mut self) {
        self.selected = 0;
    }

    fn select_last(&mut self) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = self.filtered.len() - 1;
    }
}

fn ui(f: &mut ratatui::Frame, app: &mut App) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            [
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(2),
            ]
            .as_ref(),
        )
        .split(f.area());

    let query = Paragraph::new(format!("> {}", app.query))
        .block(Block::default().borders(Borders::ALL).title("Query"))
        .style(Style::default().fg(Color::White));
    f.render_widget(query, root[0]);

    if !app.ready {
        let main = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(1)].as_ref())
            .split(root[1]);

        if app.indexing.scanning_roots && app.indexing.total_files == 0 {
            let spinners: [char; 4] = ['|', '/', '-', '\\'];
            let ch = spinners[app.spinner % spinners.len()];
            app.spinner = app.spinner.wrapping_add(1);

            let loading = Paragraph::new(format!("{ch} Scanning projects under $HOME..."))
                .block(Block::default().borders(Borders::ALL).title("Loading"))
                .wrap(Wrap { trim: false });
            f.render_widget(loading, main[0]);

            let mut lines = vec![Line::raw(format!(
                "dirs: {}   roots: {}   history_files: {}",
                app.indexing.scanned_dirs, app.indexing.found_roots, app.indexing.found_files
            ))];
            if let Some(cur) = app.indexing.scan_current.as_ref() {
                lines.push(Line::raw(format!("current: {}", cur.display())));
            }
            if let Some(w) = app.indexing.last_warn.as_deref() {
                lines.push(Line::raw(format!("warn: {w}")));
            }
            let p = Paragraph::new(Text::from(lines))
                .block(Block::default().borders(Borders::ALL).title("Status"))
                .wrap(Wrap { trim: false });
            f.render_widget(p, main[1]);
        } else {
            let pct = if app.indexing.total_files == 0 {
                0u16
            } else {
                ((app.indexing.processed_files.saturating_mul(100)) / app.indexing.total_files)
                    as u16
            };

            let gauge = Gauge::default()
                .block(Block::default().borders(Borders::ALL).title("Indexing"))
                .gauge_style(Style::default().fg(Color::Cyan))
                .percent(pct);
            f.render_widget(gauge, main[0]);

            let mut lines = vec![Line::raw(format!(
                "files: {}/{}   records: {}   sessions: {}",
                app.indexing.processed_files,
                app.indexing.total_files,
                app.indexing.records,
                app.indexing.sessions
            ))];
            if let Some(cur) = app.indexing.current.as_ref() {
                lines.push(Line::raw(format!("current: {}", cur.display())));
            }
            if let Some(w) = app.indexing.last_warn.as_deref() {
                lines.push(Line::raw(format!("warn: {w}")));
            }
            let p = Paragraph::new(Text::from(lines))
                .block(Block::default().borders(Borders::ALL).title("Status"))
                .wrap(Wrap { trim: false });
            f.render_widget(p, main[1]);
        }

        let footer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1)].as_ref())
            .split(root[2]);

        let status = Paragraph::new(
            app.indexing
                .last_warn
                .as_deref()
                .map(|s| format!("status: {s}"))
                .unwrap_or_default(),
        )
        .style(Style::default().fg(Color::Yellow));
        f.render_widget(status, footer[0]);

        let keys = Paragraph::new("Esc/Ctrl+c: quit").style(Style::default().fg(Color::DarkGray));
        f.render_widget(keys, footer[1]);
        return;
    }

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)].as_ref())
        .split(root[1]);

    // Results pane (manual windowing)
    let results_area = main[0];
    let inner_height = results_area.height.saturating_sub(2) as usize; // borders
    let total = app.filtered.len();

    if app.selected >= total && total > 0 {
        app.selected = total - 1;
    }

    if total == 0 {
        app.selected = 0;
        app.offset = 0;
    }

    if total > 0 && inner_height > 0 {
        if app.selected < app.offset {
            app.offset = app.selected;
        } else if app.selected >= app.offset + inner_height {
            app.offset = app.selected + 1 - inner_height;
        }
    }

    let visible_start = cmp::min(app.offset, total);
    let visible_end = cmp::min(visible_start + inner_height, total);

    let mut lines: Vec<Line> = Vec::new();
    for &session_idx in app.filtered[visible_start..visible_end].iter() {
        let Some(sess) = app.sessions.get(session_idx) else {
            continue;
        };
        let ts = short_ts(sess.last_ts.as_deref());

        lines.push(Line::raw(format!(
            "{} {} {} {}",
            ts,
            sess.dir,
            provider_icon(sess.source),
            sess.first_line,
        )));
    }

    let results = Paragraph::new(Text::from(lines)).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!("Results ({})", app.filtered.len())),
    );
    f.render_widget(results, results_area);

    // Preview
    let preview_area = main[1];
    let preview = if let Some(rec) = app.selected_record() {
        let source = match rec.source {
            SourceKind::CodexSessionJsonl => "codex_session",
            SourceKind::CodexHistoryJsonl => "codex_history",
            SourceKind::ClaudeProjectJsonl => "claude_project",
        };
        let role = match rec.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
            Role::Tool => "tool",
            Role::Unknown => "unknown",
        };

        let mut header: Vec<Line> = vec![
            Line::raw(format!(
                "timestamp: {}",
                rec.timestamp.as_deref().unwrap_or("")
            )),
            Line::raw(format!(
                "role: {role}   phase: {}",
                rec.phase.as_deref().unwrap_or("")
            )),
            Line::raw(format!("cwd: {}", rec.cwd.as_deref().unwrap_or(""))),
            Line::raw(format!("file: {}:{}", rec.file.display(), rec.line)),
            Line::raw(format!("source: {source}")),
            Line::raw(""),
        ];

        header.extend(rec.text.lines().map(|l| Line::raw(l.to_string())));

        Paragraph::new(Text::from(header))
            .block(Block::default().borders(Borders::ALL).title("Preview"))
            .wrap(Wrap { trim: false })
    } else {
        Paragraph::new("(no match)")
            .block(Block::default().borders(Borders::ALL).title("Preview"))
            .wrap(Wrap { trim: false })
    };
    f.render_widget(preview, preview_area);

    let footer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)].as_ref())
        .split(root[2]);

    let status = Paragraph::new(
        app.indexing
            .last_warn
            .as_deref()
            .map(|s| format!("status: {s}"))
            .unwrap_or_default(),
    )
    .style(Style::default().fg(Color::Yellow));
    f.render_widget(status, footer[0]);

    let keys = Paragraph::new(format!(
        "Esc/Ctrl+c: quit  Enter: resume  Ctrl+o: pager  Backspace: delete  Ctrl+u: clear  ↑/↓: move  query: \"{}\"",
        app.query.trim()
    ))
    .style(Style::default().fg(Color::DarkGray));
    f.render_widget(keys, footer[1]);

    // 選択行の強調（簡易：結果ペインを上書き）
    if total > 0 && inner_height > 0 && app.selected >= visible_start && app.selected < visible_end
    {
        let y = (app.selected - visible_start) as u16;
        let x = results_area.x + 1;
        let w = results_area.width.saturating_sub(2);
        let highlight_area = ratatui::layout::Rect {
            x,
            y: results_area.y + 1 + y,
            width: w,
            height: 1,
        };
        let session_idx = app.filtered[app.selected];
        let Some(sess) = app.sessions.get(session_idx) else {
            return;
        };
        let ts = short_ts(sess.last_ts.as_deref());
        let line = format!(
            "{} {} {} {}",
            ts,
            sess.dir,
            provider_icon(sess.source),
            sess.first_line
        );
        let p = Paragraph::new(line).style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        );
        f.render_widget(p, highlight_area);
    }
}

fn open_in_pager(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    rec: &MessageRecord,
) -> anyhow::Result<()> {
    let pager = env::var("PAGER").unwrap_or_else(|_| "less -R".to_string());
    let file = shell_escape(&rec.file.to_string_lossy());
    let start = rec.line.saturating_sub(40).max(1);
    let end = rec.line.saturating_add(200);
    let cmd = format!("nl -ba {file} | sed -n '{start},{end}p' | {pager}");
    let _ = run_with_tui_suspended(terminal, || {
        Command::new("sh")
            .arg("-lc")
            .arg(cmd)
            .status()
            .context("pager起動に失敗しました")
    });
    Ok(())
}

fn shell_escape(s: &str) -> String {
    let mut out = String::from("'");
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResumeTarget {
    program: String,
    args: Vec<String>,
    current_dir: Option<PathBuf>,
}

fn resume_loading_lines(target: &ResumeTarget, session_id: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    out.push(format!("resuming: {} {}", target.program, session_id));
    if let Some(cwd) = target.current_dir.as_ref() {
        out.push(format!("cwd: {}", cwd.display()));
    }
    out.push(format!(
        "command: {} {}",
        target.program,
        target.args.join(" ")
    ));
    out.push(String::new());
    out
}

fn animate_resume_loader(
    program: &str,
    session_id: &str,
    child: &mut std::process::Child,
) -> anyhow::Result<Option<std::process::ExitStatus>> {
    const FRAME_MS: u64 = 60;
    const MAX_ANIM_MS: u64 = 1500;

    let frames: [char; 4] = ['|', '/', '-', '\\'];
    let mut frame_idx: usize = 0;
    let start = Instant::now();
    let mut stderr = io::stderr();

    // 画面上で目立つように、1行だけを上書きし続ける（後続の外部CLI表示で自然に消える）
    while start.elapsed() < Duration::from_millis(MAX_ANIM_MS) {
        if let Some(st) = child
            .try_wait()
            .context("プロセス状態の取得に失敗しました")?
        {
            write!(stderr, "\r\x1b[2K")?;
            stderr.flush().ok();
            return Ok(Some(st));
        }

        let ch = frames[frame_idx % frames.len()];
        frame_idx = frame_idx.wrapping_add(1);

        // 疑似プログレス（循環バー）
        let bar_w = 24usize;
        let pos = frame_idx % (bar_w + 1);
        let mut bar = String::with_capacity(bar_w);
        for i in 0..bar_w {
            bar.push(if i == pos { '>' } else { ' ' });
        }

        write!(
            stderr,
            "\r\x1b[2K{ch} launching {program} ({session_id}) [{bar}]"
        )?;
        stderr.flush().ok();
        std::thread::sleep(Duration::from_millis(FRAME_MS));
    }

    write!(stderr, "\r\x1b[2K")?;
    stderr.flush().ok();
    Ok(None)
}

fn resume_target_for_record(app: &App, rec: &MessageRecord) -> Option<ResumeTarget> {
    let sid = rec.session_id.as_deref()?;
    let cwd = existing_dir(
        rec.cwd
            .as_deref()
            .or_else(|| find_cwd_for_session_id(app, sid)),
    );

    match rec.source {
        SourceKind::CodexSessionJsonl | SourceKind::CodexHistoryJsonl => {
            let mut args = vec!["resume".to_string()];
            if let Some(dir) = cwd.as_ref() {
                args.push("-C".to_string());
                args.push(dir.to_string_lossy().to_string());
            }
            args.push(sid.to_string());
            Some(ResumeTarget {
                program: "codex".to_string(),
                args,
                current_dir: cwd,
            })
        }
        SourceKind::ClaudeProjectJsonl => Some(ResumeTarget {
            program: "claude".to_string(),
            args: vec!["--resume".to_string(), sid.to_string()],
            current_dir: cwd,
        }),
    }
}

fn find_cwd_for_session_id<'a>(app: &'a App, session_id: &str) -> Option<&'a str> {
    app.all
        .iter()
        .filter(|r| r.session_id.as_deref() == Some(session_id))
        .filter_map(|r| r.cwd.as_deref())
        .find(|s| !s.trim().is_empty())
}

fn existing_dir(path: Option<&str>) -> Option<PathBuf> {
    let s = path?.trim();
    if s.is_empty() {
        return None;
    }
    let p = PathBuf::from(s);
    match fs::metadata(&p) {
        Ok(m) if m.is_dir() => Some(p),
        _ => None,
    }
}

fn run_with_tui_suspended<R>(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    f: impl FnOnce() -> anyhow::Result<R>,
) -> anyhow::Result<R> {
    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen, cursor::Show).ok();
    terminal.show_cursor().ok();

    let res = f();

    enable_raw_mode().ok();
    execute!(terminal.backend_mut(), EnterAlternateScreen, cursor::Hide).ok();
    terminal.hide_cursor().ok();
    terminal.clear().ok();

    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(prefix: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!("{prefix}-{nanos}-{}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            Self { path: dir }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn resume_target_for_codex_uses_codex_resume_with_cd_when_cwd_exists() {
        let tmp = TempDir::new("agent-history");
        let cwd = tmp.path.join("proj");
        fs::create_dir_all(&cwd).unwrap();

        let rec = MessageRecord {
            timestamp: None,
            role: Role::User,
            text: "x".to_string(),
            file: PathBuf::from("/tmp/x.jsonl"),
            line: 1,
            session_id: Some("019c5a97-1de5-7371-80ef-72ae0f764f43".to_string()),
            cwd: Some(cwd.to_string_lossy().to_string()),
            phase: None,
            source: SourceKind::CodexSessionJsonl,
        };

        let app = App {
            query: String::new(),
            max_results: 0,
            all: vec![rec.clone()],
            sessions: Vec::new(),
            session_records: Vec::new(),
            filtered: vec![],
            selected: 0,
            offset: 0,
            last_query: String::new(),
            last_results: vec![],
            indexing: IndexingProgress::default(),
            ready: true,
            spinner: 0,
        };

        let target = resume_target_for_record(&app, &rec).unwrap();
        assert_eq!(target.program, "codex");
        assert_eq!(
            target.args,
            vec![
                "resume".to_string(),
                "-C".to_string(),
                cwd.to_string_lossy().to_string(),
                "019c5a97-1de5-7371-80ef-72ae0f764f43".to_string(),
            ]
        );
        assert_eq!(target.current_dir.as_deref(), Some(cwd.as_path()));
    }

    #[test]
    fn resume_target_for_claude_uses_claude_resume() {
        let tmp = TempDir::new("agent-history");
        let cwd = tmp.path.join("proj");
        fs::create_dir_all(&cwd).unwrap();

        let rec = MessageRecord {
            timestamp: None,
            role: Role::User,
            text: "x".to_string(),
            file: PathBuf::from("/tmp/x.jsonl"),
            line: 1,
            session_id: Some("8adefc6b-d73e-4a0b-a330-9be4114a5bdb".to_string()),
            cwd: Some(cwd.to_string_lossy().to_string()),
            phase: None,
            source: SourceKind::ClaudeProjectJsonl,
        };

        let app = App {
            query: String::new(),
            max_results: 0,
            all: vec![rec.clone()],
            sessions: Vec::new(),
            session_records: Vec::new(),
            filtered: vec![],
            selected: 0,
            offset: 0,
            last_query: String::new(),
            last_results: vec![],
            indexing: IndexingProgress::default(),
            ready: true,
            spinner: 0,
        };

        let target = resume_target_for_record(&app, &rec).unwrap();
        assert_eq!(target.program, "claude");
        assert_eq!(
            target.args,
            vec![
                "--resume".to_string(),
                "8adefc6b-d73e-4a0b-a330-9be4114a5bdb".to_string(),
            ]
        );
        assert_eq!(target.current_dir.as_deref(), Some(cwd.as_path()));
    }

    #[test]
    fn resume_loading_lines_includes_command_and_optional_cwd() {
        let target = ResumeTarget {
            program: "codex".to_string(),
            args: vec![
                "resume".to_string(),
                "-C".to_string(),
                "/x".to_string(),
                "sid".to_string(),
            ],
            current_dir: Some(PathBuf::from("/x")),
        };

        let lines = resume_loading_lines(&target, "sid");
        assert_eq!(lines[0], "resuming: codex sid");
        assert_eq!(lines[1], "cwd: /x");
        assert_eq!(lines[2], "command: codex resume -C /x sid");
    }

    fn mr(
        ts: Option<&str>,
        role: Role,
        text: &str,
        session_id: &str,
        source: SourceKind,
    ) -> MessageRecord {
        MessageRecord {
            timestamp: ts.map(|s| s.to_string()),
            role,
            text: text.to_string(),
            file: PathBuf::from("/tmp/x.jsonl"),
            line: 1,
            session_id: Some(session_id.to_string()),
            cwd: None,
            phase: None,
            source,
        }
    }

    #[test]
    fn build_session_index_uses_first_user_message_and_sorts_by_last_activity() {
        let all = vec![
            mr(
                Some("2026-02-10T00:00:00Z"),
                Role::System,
                "sys",
                "a",
                SourceKind::CodexSessionJsonl,
            ),
            mr(
                Some("2026-02-10T00:00:01Z"),
                Role::User,
                "first hello",
                "a",
                SourceKind::CodexSessionJsonl,
            ),
            mr(
                Some("2026-02-11T00:00:00Z"),
                Role::Assistant,
                "needle",
                "a",
                SourceKind::CodexSessionJsonl,
            ),
            mr(
                Some("2026-02-12T00:00:00Z"),
                Role::User,
                "yo",
                "b",
                SourceKind::ClaudeProjectJsonl,
            ),
            mr(
                Some("2026-02-13T00:00:00Z"),
                Role::Assistant,
                "ok",
                "b",
                SourceKind::ClaudeProjectJsonl,
            ),
        ];

        let (sessions, records) = build_session_index(&all);
        assert_eq!(sessions.len(), 2);
        assert_eq!(records.len(), 2);

        // 最新の会話が先頭
        assert_eq!(sessions[0].session_id, "b");
        assert_eq!(sessions[0].source, SourceKind::ClaudeProjectJsonl);
        assert_eq!(all[sessions[0].first_user_idx].role, Role::User);
        assert_eq!(all[sessions[0].first_user_idx].text, "yo");
        assert_eq!(records[0].len(), 2);

        // 2つ目の会話
        assert_eq!(sessions[1].session_id, "a");
        assert_eq!(sessions[1].source, SourceKind::CodexSessionJsonl);
        assert_eq!(all[sessions[1].first_user_idx].role, Role::User);
        assert_eq!(all[sessions[1].first_user_idx].text, "first hello");
        assert_eq!(records[1].len(), 3);
    }

    #[test]
    fn update_results_matches_any_message_but_selected_record_is_first_user_message() {
        let all = vec![
            mr(
                Some("2026-02-10T00:00:00Z"),
                Role::System,
                "sys",
                "a",
                SourceKind::CodexSessionJsonl,
            ),
            mr(
                Some("2026-02-10T00:00:01Z"),
                Role::User,
                "first hello",
                "a",
                SourceKind::CodexSessionJsonl,
            ),
            mr(
                Some("2026-02-11T00:00:00Z"),
                Role::Assistant,
                "hay needle stack",
                "a",
                SourceKind::CodexSessionJsonl,
            ),
            mr(
                Some("2026-02-12T00:00:00Z"),
                Role::User,
                "yo",
                "b",
                SourceKind::ClaudeProjectJsonl,
            ),
            mr(
                Some("2026-02-13T00:00:00Z"),
                Role::Assistant,
                "ok",
                "b",
                SourceKind::ClaudeProjectJsonl,
            ),
        ];
        let (sessions, session_records) = build_session_index(&all);
        let mut app = App {
            query: "needle".to_string(),
            max_results: 0,
            all,
            sessions,
            session_records,
            filtered: vec![],
            selected: 0,
            offset: 0,
            last_query: String::new(),
            last_results: vec![],
            indexing: IndexingProgress::default(),
            ready: true,
            spinner: 0,
        };

        app.update_results();
        assert_eq!(app.filtered.len(), 1);

        let sess = app.selected_session().unwrap();
        assert_eq!(sess.session_id, "a");

        let rec = app.selected_record().unwrap();
        assert_eq!(rec.role, Role::User);
        assert_eq!(rec.text, "first hello");
    }

    #[test]
    fn provider_icon_distinguishes_claude_and_openai() {
        assert_eq!(provider_icon(SourceKind::ClaudeProjectJsonl), "C");
        assert_eq!(provider_icon(SourceKind::CodexSessionJsonl), "O");
        assert_eq!(provider_icon(SourceKind::CodexHistoryJsonl), "O");
    }

    #[test]
    fn build_session_index_extracts_user_prompt_from_codex_title_task_prompt() {
        let all = vec![
            mr(
                Some("2026-02-10T00:00:00Z"),
                Role::User,
                "# AGENTS.md instructions for /x\n\n<INSTRUCTIONS>\n...",
                "a",
                SourceKind::CodexSessionJsonl,
            ),
            mr(
                Some("2026-02-10T00:00:01Z"),
                Role::User,
                "<environment_context>\n  <cwd>/x</cwd>\n</environment_context>",
                "a",
                SourceKind::CodexSessionJsonl,
            ),
            mr(
                Some("2026-02-10T00:00:02Z"),
                Role::User,
                "You are a helpful assistant. You will be presented with a user prompt.\n\nUser prompt:\nline1\nline2",
                "a",
                SourceKind::CodexSessionJsonl,
            ),
        ];

        let (sessions, _) = build_session_index(&all);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].first_line, "line2");
    }

    #[test]
    fn build_session_index_skips_agents_instructions_in_codex_sessions() {
        let all = vec![
            mr(
                Some("2026-02-10T00:00:00Z"),
                Role::User,
                "# AGENTS.md instructions for /x\n\n<INSTRUCTIONS>\n...",
                "a",
                SourceKind::CodexSessionJsonl,
            ),
            mr(
                Some("2026-02-10T00:00:01Z"),
                Role::User,
                "real first",
                "a",
                SourceKind::CodexSessionJsonl,
            ),
            mr(
                Some("2026-02-10T00:00:02Z"),
                Role::Assistant,
                "ok",
                "a",
                SourceKind::CodexSessionJsonl,
            ),
        ];

        let (sessions, _) = build_session_index(&all);
        assert_eq!(sessions.len(), 1);

        let first = &all[sessions[0].first_user_idx];
        assert_eq!(first.role, Role::User);
        assert_eq!(first.text, "real first");
    }

    #[test]
    fn build_session_index_skips_environment_context_in_codex_sessions() {
        let all = vec![
            mr(
                Some("2026-02-10T00:00:00Z"),
                Role::User,
                "<environment_context>\n  <cwd>/x</cwd>\n  <shell>zsh</shell>\n</environment_context>",
                "a",
                SourceKind::CodexSessionJsonl,
            ),
            mr(
                Some("2026-02-10T00:00:01Z"),
                Role::User,
                "real first",
                "a",
                SourceKind::CodexSessionJsonl,
            ),
        ];

        let (sessions, _) = build_session_index(&all);
        assert_eq!(sessions.len(), 1);

        let first = &all[sessions[0].first_user_idx];
        assert_eq!(first.role, Role::User);
        assert_eq!(first.text, "real first");
    }

    #[test]
    fn build_session_index_skips_sessions_with_only_noise_user_messages() {
        let all = vec![
            mr(
                Some("2026-02-10T00:00:00Z"),
                Role::User,
                "# AGENTS.md instructions for /x\n\n<INSTRUCTIONS>\n...",
                "a",
                SourceKind::CodexSessionJsonl,
            ),
            mr(
                Some("2026-02-10T00:00:01Z"),
                Role::User,
                "<environment_context>\n  <cwd>/x</cwd>\n</environment_context>",
                "a",
                SourceKind::CodexSessionJsonl,
            ),
            mr(
                Some("2026-02-10T00:00:02Z"),
                Role::Assistant,
                "ok",
                "a",
                SourceKind::CodexSessionJsonl,
            ),
        ];

        let (sessions, _) = build_session_index(&all);
        assert!(sessions.is_empty());
    }

    #[test]
    fn build_session_index_sets_dir_from_cwd() {
        let mut r1 = mr(
            Some("2026-02-10T00:00:00Z"),
            Role::User,
            "hi",
            "a",
            SourceKind::CodexSessionJsonl,
        );
        r1.cwd = Some("/home/tizze/projects/myproj".to_string());
        let all = vec![r1];

        let (sessions, _) = build_session_index(&all);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].dir, "myproj");
    }

    #[test]
    fn build_session_index_includes_codex_history_sessions() {
        let all = vec![
            mr(
                Some("100"),
                Role::Unknown,
                "hello",
                "h1",
                SourceKind::CodexHistoryJsonl,
            ),
            mr(
                Some("200"),
                Role::Unknown,
                "later",
                "h1",
                SourceKind::CodexHistoryJsonl,
            ),
        ];

        let (sessions, _) = build_session_index(&all);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].source, SourceKind::CodexHistoryJsonl);
        assert_eq!(sessions[0].session_id, "h1");
        assert_eq!(all[sessions[0].first_user_idx].text, "hello");
    }

    #[test]
    fn dir_name_from_cwd_extracts_basename() {
        assert_eq!(
            dir_name_from_cwd("/home/tizze/ghq/github.com/tizze/agent-history"),
            "agent-history"
        );
        assert_eq!(dir_name_from_cwd("/home/tizze/x/y/"), "y");
        assert_eq!(dir_name_from_cwd(""), "-");
        assert_eq!(dir_name_from_cwd("   "), "-");
    }
}
