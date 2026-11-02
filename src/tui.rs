//! The interactive dashboard: `tidyup` with no arguments.

use std::{
    collections::HashSet,
    io::IsTerminal,
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Flex, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Cell, Clear, Paragraph, Row, Table, TableState, Tabs},
};
use rayon::prelude::*;

use crate::{
    caches,
    clean::{self, Junk},
    fmt,
    ports::{self, Listener, Outcome},
    receipt,
    strays::{self, StopReport, Stray},
};

const REFRESH: Duration = Duration::from_secs(2);
const STATUS_TTL: Duration = Duration::from_secs(6);

pub fn run(root: PathBuf) -> Result<()> {
    if !std::io::stdout().is_terminal() {
        bail!("the dashboard needs a terminal; see `tidyup --help` for plain commands");
    }
    let mut terminal = ratatui::init();
    let result = App::new(root).run(&mut terminal);
    ratatui::restore();
    result
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Ports,
    Clean,
    Strays,
}

/// Column the Clean tab is sorted by.
#[derive(Clone, Copy, PartialEq)]
enum SortBy {
    Size,
    Age,
}

/// Results coming back from background threads.
enum Msg {
    Ports(Result<Vec<Listener>, String>),
    Strays(Result<Vec<Stray>, String>),
    StraysStopped {
        stray: Stray,
        report: StopReport,
    },
    Found {
        scan: u64,
        junk: Junk,
    },
    ScanDone {
        scan: u64,
    },
    Stopped {
        listener: Listener,
        result: Result<Outcome, String>,
    },
    Deleted {
        path: PathBuf,
        size: u64,
        result: Result<(), String>,
    },
}

struct App {
    tab: Tab,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    quit: bool,
    status: Option<(String, Instant)>,

    ports: Vec<Listener>,
    show_all: bool,
    ports_table: TableState,
    refreshing: bool,
    refreshed: Option<Instant>,

    root: PathBuf,
    junk: Vec<Junk>,
    marked: HashSet<PathBuf>,
    deleting: HashSet<PathBuf>,
    junk_table: TableState,
    sort: SortBy,
    /// Whether the Clean tab also lists global caches.
    caches: bool,
    /// Biggest / oldest first.
    descending: bool,
    /// Id of the current scan, so results from a replaced scan are ignored.
    scan: u64,
    scanning: bool,
    /// Folders waiting for a yes/no before deletion.
    confirm: Option<Vec<PathBuf>>,

    strays: Vec<Stray>,
    strays_table: TableState,
    strays_refreshed: bool,
    /// Pids being stopped, so a refresh doesn't offer them again meanwhile.
    stopping: HashSet<u32>,

    /// What this session got back, shown in the header.
    session: Session,
}

/// Running totals for the header: what you've freed since opening the dashboard.
#[derive(Default)]
struct Session {
    bytes: u64,
    ports: usize,
    /// CPU of stopped strays, in % of one core.
    cpu: f32,
}

impl Session {
    /// `✔ 41.8 GB · 2 ports · 3.6 cores freed this session`, or nothing yet.
    fn text(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.bytes > 0 {
            parts.push(fmt::size(self.bytes));
        }
        if self.ports > 0 {
            parts.push(format!(
                "{} port{}",
                self.ports,
                if self.ports == 1 { "" } else { "s" }
            ));
        }
        if self.cpu >= 1.0 {
            parts.push(receipt::cpu_text(self.cpu));
        }
        (!parts.is_empty()).then(|| format!("✔ {} freed this session ", parts.join(" · ")))
    }
}

impl App {
    fn new(root: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tab: Tab::Ports,
            tx,
            rx,
            quit: false,
            status: None,
            ports: Vec::new(),
            show_all: false,
            ports_table: TableState::default().with_selected(0),
            refreshing: false,
            refreshed: None,
            root,
            junk: Vec::new(),
            marked: HashSet::new(),
            deleting: HashSet::new(),
            junk_table: TableState::default().with_selected(0),
            sort: SortBy::Size,
            caches: false,
            descending: true,
            scan: 0,
            scanning: false,
            confirm: None,
            strays: Vec::new(),
            strays_table: TableState::default().with_selected(0),
            strays_refreshed: false,
            stopping: HashSet::new(),
            session: Session::default(),
        }
    }

    fn run(mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        self.start_scan();
        while !self.quit {
            self.receive();
            if !self.refreshing && self.refreshed.is_none_or(|at| at.elapsed() >= REFRESH) {
                self.refresh_ports();
            }
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(Duration::from_millis(100))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                self.on_key(key);
            }
        }
        Ok(())
    }

    // ── background work ────────────────────────────────────────────────

    /// Refreshes ports and strays together: both come from `ps`/`lsof`.
    fn refresh_ports(&mut self) {
        self.refreshing = true;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let _ = tx.send(Msg::Ports(ports::list().map_err(|e| format!("{e:#}"))));
            let _ = tx.send(Msg::Strays(strays::list().map_err(|e| format!("{e:#}"))));
        });
    }

    fn stop_selected_stray(&mut self, force: bool) {
        let Some(stray) = self.selected_stray().cloned() else {
            return;
        };
        if stray.pids.iter().any(|pid| self.stopping.contains(pid)) {
            return;
        }
        self.stopping.extend(stray.pids.iter().copied());
        self.set_status(format!("Stopping {}…", stray.label()));
        let tx = self.tx.clone();
        thread::spawn(move || {
            let report = strays::stop(&stray, force);
            let _ = tx.send(Msg::StraysStopped { stray, report });
        });
    }

    fn start_scan(&mut self) {
        self.scan += 1;
        self.scanning = true;
        self.junk.clear();
        self.marked.clear();
        let (scan, root, tx) = (self.scan, self.root.clone(), self.tx.clone());
        let include_caches = self.caches;
        thread::spawn(move || {
            let send = |junk: Junk| {
                if junk.size > 0 {
                    let _ = tx.send(Msg::Found { scan, junk });
                }
            };
            if include_caches {
                rayon::join(|| clean::scan(&root, send), || caches::scan(send));
            } else {
                clean::scan(&root, send);
            }
            let _ = tx.send(Msg::ScanDone { scan });
        });
    }

    fn stop_selected(&mut self, force: bool) {
        let Some(listener) = self.selected_port().cloned() else {
            return;
        };
        self.set_status(format!(
            "Stopping {} on :{}…",
            listener.label(),
            listener.port
        ));
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = ports::stop_listener(&listener, force).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Stopped { listener, result });
        });
    }

    fn delete(&mut self, paths: Vec<PathBuf>) {
        let jobs: Vec<Junk> = self
            .junk
            .iter()
            .filter(|j| paths.contains(&j.path))
            .cloned()
            .collect();
        for junk in &jobs {
            self.marked.remove(&junk.path);
            self.deleting.insert(junk.path.clone());
        }
        let tx = self.tx.clone();
        // Separate folders delete fastest side by side; see `clean::delete_counting`.
        thread::spawn(move || {
            jobs.into_par_iter().for_each(|junk| {
                let result = clean::delete(&junk).map_err(|e| e.to_string());
                let (path, size) = (junk.path, junk.size);
                let _ = tx.send(Msg::Deleted { path, size, result });
            });
        });
    }

    fn receive(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Ports(result) => {
                    self.refreshed = Some(Instant::now());
                    match result {
                        Ok(ports) => self.ports = ports,
                        Err(err) => self.set_status(err),
                    }
                }
                Msg::Strays(result) => {
                    self.refreshing = false;
                    self.strays_refreshed = true;
                    match result {
                        Ok(strays) => self.strays = strays,
                        Err(err) => self.set_status(err),
                    }
                }
                Msg::StraysStopped { stray, report } => {
                    for pid in &stray.pids {
                        self.stopping.remove(pid);
                    }
                    let name = stray.label();
                    if report.failed.is_empty() {
                        self.session.cpu += stray.cpu;
                    }
                    self.set_status(if !report.failed.is_empty() {
                        format!("Could not stop all of {name}: {}", report.failed.join("; "))
                    } else if report.forced > 0 {
                        format!("Force-killed {name}")
                    } else {
                        format!("Stopped {name}")
                    });
                    self.refresh_ports();
                }
                Msg::Found { scan, junk } if scan == self.scan => {
                    self.junk.push(junk);
                    self.sort_junk();
                }
                Msg::ScanDone { scan } if scan == self.scan => self.scanning = false,
                Msg::Found { .. } | Msg::ScanDone { .. } => {}
                Msg::Stopped { listener, result } => {
                    let name = format!("{} on :{}", listener.label(), listener.port);
                    if result.is_ok() {
                        self.session.ports += 1;
                    }
                    self.set_status(match result {
                        Ok(Outcome::Stopped) => format!("Stopped {name}"),
                        Ok(Outcome::Forced) => format!("Force-killed {name}"),
                        Err(err) => format!("Could not stop {name}: {err}"),
                    });
                    self.refresh_ports();
                }
                Msg::Deleted { path, size, result } => {
                    self.deleting.remove(&path);
                    let shown = clean::display(&path, &self.root);
                    match result {
                        Ok(()) => {
                            self.session.bytes += size;
                            self.junk.retain(|j| j.path != path);
                            self.set_status(format!("Deleted {shown}, freed {}", fmt::size(size)));
                        }
                        Err(err) => self.set_status(format!("Could not delete {shown}: {err}")),
                    }
                }
            }
        }
    }

    // ── input ──────────────────────────────────────────────────────────

    fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if let Some(paths) = self.confirm.take() {
            if matches!(key.code, KeyCode::Char('y') | KeyCode::Enter) {
                self.delete(paths);
            }
            return;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Tab => {
                self.tab = match self.tab {
                    Tab::Ports => Tab::Clean,
                    Tab::Clean => Tab::Strays,
                    Tab::Strays => Tab::Ports,
                }
            }
            KeyCode::BackTab => {
                self.tab = match self.tab {
                    Tab::Ports => Tab::Strays,
                    Tab::Clean => Tab::Ports,
                    Tab::Strays => Tab::Clean,
                }
            }
            KeyCode::Char('1') => self.tab = Tab::Ports,
            KeyCode::Char('2') => self.tab = Tab::Clean,
            KeyCode::Char('3') => self.tab = Tab::Strays,
            KeyCode::Down | KeyCode::Char('j') => self.table().select_next(),
            KeyCode::Up | KeyCode::Char('k') => self.table().select_previous(),
            KeyCode::Home | KeyCode::Char('g') => self.table().select_first(),
            KeyCode::End | KeyCode::Char('G') => self.table().select_last(),
            code => match self.tab {
                Tab::Ports => self.on_ports_key(code),
                Tab::Clean => self.on_clean_key(code),
                Tab::Strays => self.on_strays_key(code),
            },
        }
    }

    fn on_ports_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('x') => self.stop_selected(false),
            KeyCode::Char('X') => self.stop_selected(true),
            KeyCode::Char('a') => self.show_all = !self.show_all,
            KeyCode::Char('r') => self.refresh_ports(),
            _ => {}
        }
    }

    fn on_strays_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('x') => self.stop_selected_stray(false),
            KeyCode::Char('X') => self.stop_selected_stray(true),
            KeyCode::Char('r') => self.refresh_ports(),
            _ => {}
        }
    }

    fn on_clean_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char(' ') => {
                if let Some(path) = self.selected_junk().map(|j| j.path.clone()) {
                    if !self.marked.remove(&path) {
                        self.marked.insert(path);
                    }
                    self.junk_table.select_next();
                }
            }
            KeyCode::Char('a') => {
                if self.marked.len() == self.junk.len() {
                    self.marked.clear();
                } else {
                    self.marked = self.junk.iter().map(|j| j.path.clone()).collect();
                }
            }
            KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => {
                let paths: Vec<PathBuf> = if self.marked.is_empty() {
                    self.selected_junk()
                        .map(|j| j.path.clone())
                        .into_iter()
                        .collect()
                } else {
                    self.marked.iter().cloned().collect()
                };
                let paths: Vec<PathBuf> = paths
                    .into_iter()
                    .filter(|p| !self.deleting.contains(p))
                    .collect();
                if !paths.is_empty() {
                    self.confirm = Some(paths);
                }
            }
            KeyCode::Char('r') => self.start_scan(),
            KeyCode::Char('c') => {
                self.caches = !self.caches;
                self.start_scan();
            }
            KeyCode::Char('s') => self.sort_by(SortBy::Size),
            KeyCode::Char('t') => self.sort_by(SortBy::Age),
            _ => {}
        }
    }

    // ── state helpers ──────────────────────────────────────────────────

    fn table(&mut self) -> &mut TableState {
        match self.tab {
            Tab::Ports => &mut self.ports_table,
            Tab::Clean => &mut self.junk_table,
            Tab::Strays => &mut self.strays_table,
        }
    }

    fn selected_stray(&self) -> Option<&Stray> {
        self.strays.get(self.strays_table.selected()?)
    }

    fn visible_ports(&self) -> Vec<&Listener> {
        self.ports
            .iter()
            .filter(|l| self.show_all || l.is_dev())
            .collect()
    }

    fn selected_port(&self) -> Option<&Listener> {
        self.visible_ports()
            .get(self.ports_table.selected()?)
            .copied()
    }

    fn selected_junk(&self) -> Option<&Junk> {
        self.junk.get(self.junk_table.selected()?)
    }

    /// Sorts by `column`, or flips the direction if it's already the sort column.
    fn sort_by(&mut self, column: SortBy) {
        if self.sort == column {
            self.descending = !self.descending;
        } else {
            self.sort = column;
            self.descending = true;
        }
        self.sort_junk();
    }

    /// Re-sorts the Clean list, keeping the same folder selected.
    fn sort_junk(&mut self) {
        let selected = self.selected_junk().map(|j| j.path.clone());
        let (sort, descending) = (self.sort, self.descending);
        self.junk.sort_by(|a, b| {
            let order = match sort {
                SortBy::Size => a.size.cmp(&b.size),
                // Older means more age, so compare timestamps the other way round.
                SortBy::Age => b.last_used.cmp(&a.last_used),
            };
            let order = if descending { order.reverse() } else { order };
            order.then_with(|| b.size.cmp(&a.size))
        });
        if let Some(selected) = selected {
            let index = self.junk.iter().position(|j| j.path == selected);
            self.junk_table.select(index);
        }
    }

    fn set_status(&mut self, text: String) {
        self.status = Some((text, Instant::now()));
    }

    // ── drawing ────────────────────────────────────────────────────────

    fn draw(&mut self, frame: &mut Frame) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        self.draw_header(frame, header);
        match self.tab {
            Tab::Ports => self.draw_ports(frame, body),
            Tab::Clean => self.draw_clean(frame, body),
            Tab::Strays => self.draw_strays(frame, body),
        }
        self.draw_footer(frame, footer);
        if let Some(paths) = &self.confirm {
            self.draw_confirm(frame, paths);
        }
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let tally = self.session.text().unwrap_or_default();
        let [title, tabs, totals] = Layout::horizontal([
            Constraint::Length(8),
            Constraint::Min(0),
            Constraint::Length(tally.chars().count() as u16),
        ])
        .areas(area);
        frame.render_widget(Line::from(tally).green().right_aligned(), totals);
        frame.render_widget(Span::from(" tidyup").bold().cyan(), title);
        let selected = match self.tab {
            Tab::Ports => 0,
            Tab::Clean => 1,
            Tab::Strays => 2,
        };
        // A hot stray is why you opened tidyup: mark the tab so it's seen from anywhere.
        let strays_tab = if self.strays.iter().any(Stray::is_hot) {
            Line::from(vec![Span::from("3 Strays "), Span::from("●").red()])
        } else {
            Line::from("3 Strays")
        };
        frame.render_widget(
            Tabs::new([Line::from("1 Ports"), Line::from("2 Clean"), strays_tab])
                .select(selected)
                .highlight_style(Style::new().bold().reversed())
                .divider("│"),
            tabs,
        );
    }

    fn draw_ports(&mut self, frame: &mut Frame, area: Rect) {
        let visible = self.visible_ports();
        let hidden = self.ports.len() - visible.len();
        let mut title = format!(" Listening ports · {} ", visible.len());
        if hidden > 0 {
            title.push_str(&format!("· {hidden} system hidden "));
        }
        let block = Block::bordered().title(title);

        if visible.is_empty() {
            let text = if self.refreshed.is_none() {
                "Looking…"
            } else {
                "Nothing is listening."
            };
            frame.render_widget(Paragraph::new(text).dim().block(block), area);
            return;
        }

        let name_width = visible
            .iter()
            .map(|l| l.label().len())
            .max()
            .unwrap_or(0)
            .clamp(7, 32);
        let rows: Vec<Row> = visible
            .iter()
            .map(|l| {
                Row::new([
                    Cell::from(l.port.to_string()).style(Style::new().cyan().bold()),
                    Cell::from(l.label()),
                    Cell::from(l.pid.to_string()),
                    Cell::from(l.stats().0),
                    Cell::from(l.stats().1),
                    Cell::from(l.folder()),
                ])
            })
            .collect();
        let table = Table::new(
            rows,
            [
                Constraint::Length(6),
                Constraint::Length(name_width as u16),
                Constraint::Length(7),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Fill(1),
            ],
        )
        .header(Row::new(["PORT", "PROCESS", "PID", "UPTIME", "MEM", "FOLDER"]).dim())
        .block(block)
        .row_highlight_style(Style::new().reversed())
        .highlight_symbol("› ");
        frame.render_stateful_widget(table, area, &mut self.ports_table);
    }

    fn draw_strays(&mut self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered().title(format!(
            " Leftover dev processes · {} ",
            strays::summary(&self.strays)
        ));
        if self.strays.is_empty() {
            let text = if self.strays_refreshed {
                "No leftover dev processes."
            } else {
                "Looking…"
            };
            frame.render_widget(Paragraph::new(text).dim().block(block), area);
            return;
        }

        let rows: Vec<Row> = self
            .strays
            .iter()
            .map(|s| {
                let busy = s.pids.iter().any(|pid| self.stopping.contains(pid));
                let (mark, style) = if busy {
                    ("…", Style::new().dim())
                } else if s.is_hot() {
                    ("●", Style::new().red())
                } else {
                    (" ", Style::new())
                };
                Row::new([
                    Cell::from(mark),
                    Cell::from(s.cpu_text()).style(Style::new().bold()),
                    Cell::from(fmt::size(s.memory)),
                    Cell::from(fmt::duration(s.uptime)),
                    Cell::from(s.label()),
                    Cell::from(s.folder()),
                ])
                .style(style)
            })
            .collect();
        let table = Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Length(10),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(30),
                Constraint::Fill(1),
            ],
        )
        .header(Row::new(["", "CPU", "MEM", "RUNNING", "WHAT", "PROJECT"]).dim())
        .block(block)
        .row_highlight_style(Style::new().reversed())
        .highlight_symbol("› ");
        frame.render_stateful_widget(table, area, &mut self.strays_table);
    }

    fn draw_clean(&mut self, frame: &mut Frame, area: Rect) {
        let total: u64 = self.junk.iter().map(|j| j.size).sum();
        let mut title = format!(
            " {} · {} folders · {} ",
            fmt::path(&self.root),
            self.junk.len(),
            fmt::size(total)
        );
        if !self.marked.is_empty() {
            let marked: u64 = self
                .junk
                .iter()
                .filter(|j| self.marked.contains(&j.path))
                .map(|j| j.size)
                .sum();
            title.push_str(&format!(
                "· {} marked, {} ",
                self.marked.len(),
                fmt::size(marked)
            ));
        }
        if self.scanning {
            title.push_str("· scanning… ");
        }
        let block = Block::bordered().title(title);

        if self.junk.is_empty() {
            let text = if self.scanning {
                "Scanning…"
            } else {
                "Nothing to clean here."
            };
            frame.render_widget(Paragraph::new(text).dim().block(block), area);
            return;
        }

        let rows: Vec<Row> = self
            .junk
            .iter()
            .map(|j| {
                let (mark, style) = if self.deleting.contains(&j.path) {
                    ("…", Style::new().dim())
                } else if self.marked.contains(&j.path) {
                    ("●", Style::new().yellow())
                } else {
                    ("○", Style::new())
                };
                Row::new([
                    Cell::from(mark),
                    Cell::from(fmt::size(j.size)).style(Style::new().bold()),
                    Cell::from(fmt::age(j.last_used)),
                    Cell::from(j.kind),
                    Cell::from(clean::display(&j.path, &self.root)),
                ])
                .style(style)
            })
            .collect();
        let table = Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Length(8),
                Constraint::Length(5),
                Constraint::Length(10),
                Constraint::Fill(1),
            ],
        )
        .header(
            Row::new({
                let arrow = if self.descending { "↓" } else { "↑" };
                let (size, age) = match self.sort {
                    SortBy::Size => (format!("SIZE {arrow}"), "AGE".to_string()),
                    SortBy::Age => ("SIZE".to_string(), format!("AGE {arrow}")),
                };
                [
                    String::new(),
                    size,
                    age,
                    "KIND".to_string(),
                    "PATH".to_string(),
                ]
            })
            .dim(),
        )
        .block(block)
        .row_highlight_style(Style::new().reversed())
        .highlight_symbol("› ");
        frame.render_stateful_widget(table, area, &mut self.junk_table);
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        if let Some((text, at)) = &self.status
            && at.elapsed() < STATUS_TTL
        {
            frame.render_widget(Line::from(format!(" {text}")).yellow(), area);
            return;
        }
        let keys: &[(&str, &str)] = match self.tab {
            Tab::Ports => &[
                ("↑↓", "move"),
                ("x", "stop"),
                ("X", "force kill"),
                (
                    "a",
                    if self.show_all {
                        "hide system"
                    } else {
                        "show system"
                    },
                ),
                ("tab", "switch"),
                ("q", "quit"),
            ],
            Tab::Clean => &[
                ("↑↓", "move"),
                ("space", "mark"),
                ("a", "mark all"),
                ("d", "delete"),
                (
                    "s",
                    if self.sort == SortBy::Size {
                        "flip size"
                    } else {
                        "sort size"
                    },
                ),
                (
                    "t",
                    if self.sort == SortBy::Age {
                        "flip age"
                    } else {
                        "sort age"
                    },
                ),
                ("c", if self.caches { "hide caches" } else { "caches" }),
                ("r", "rescan"),
                ("tab", "switch"),
                ("q", "quit"),
            ],
            Tab::Strays => &[
                ("↑↓", "move"),
                ("x", "stop group"),
                ("X", "force kill"),
                ("r", "refresh"),
                ("tab", "switch"),
                ("q", "quit"),
            ],
        };
        let spans: Vec<Span> = keys
            .iter()
            .flat_map(|(key, label)| {
                [
                    Span::from(format!(" {key} ")).bold(),
                    Span::from(format!("{label} ")).dim(),
                ]
            })
            .collect();
        frame.render_widget(Line::from(spans), area);
    }

    fn draw_confirm(&self, frame: &mut Frame, paths: &[PathBuf]) {
        let size: u64 = self
            .junk
            .iter()
            .filter(|j| paths.contains(&j.path))
            .map(|j| j.size)
            .sum();
        let what = match paths {
            [one] => clean::display(one, &self.root),
            _ => format!("{} folders", paths.len()),
        };
        let area = centered(frame.area(), 60, 5);
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(format!("Delete {what} ({})?", fmt::size(size))).bold(),
                Line::from(""),
                Line::from("y / enter to delete · any other key to cancel").dim(),
            ])
            .centered()
            .block(
                Block::bordered()
                    .title(" Delete ")
                    .border_style(Style::new().fg(Color::Red)),
            ),
            area,
        );
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [area] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(area);
    let [area] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    area
}

#[cfg(test)]
mod tests {
    use super::Session;

    #[test]
    fn session_tally_lists_only_what_was_freed() {
        let mut session = Session::default();
        assert_eq!(session.text(), None);
        session.ports = 1;
        assert_eq!(session.text().unwrap(), "✔ 1 port freed this session ");
        session.bytes = 41_800_000_000;
        session.ports = 2;
        session.cpu = 360.0;
        assert_eq!(
            session.text().unwrap(),
            "✔ 42 GB · 2 ports · 3.6 cores freed this session "
        );
    }
}
