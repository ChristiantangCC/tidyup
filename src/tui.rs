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

use crate::{
    clean::{self, Junk},
    fmt,
    ports::{self, Listener, Outcome},
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
    /// Biggest / oldest first.
    descending: bool,
    /// Id of the current scan, so results from a replaced scan are ignored.
    scan: u64,
    scanning: bool,
    /// Folders waiting for a yes/no before deletion.
    confirm: Option<Vec<PathBuf>>,
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
            descending: true,
            scan: 0,
            scanning: false,
            confirm: None,
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

    fn refresh_ports(&mut self) {
        self.refreshing = true;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let _ = tx.send(Msg::Ports(ports::list().map_err(|e| format!("{e:#}"))));
        });
    }

    fn start_scan(&mut self) {
        self.scan += 1;
        self.scanning = true;
        self.junk.clear();
        self.marked.clear();
        let (scan, root, tx) = (self.scan, self.root.clone(), self.tx.clone());
        thread::spawn(move || {
            clean::scan(&root, |junk| {
                if junk.size > 0 {
                    let _ = tx.send(Msg::Found { scan, junk });
                }
            });
            let _ = tx.send(Msg::ScanDone { scan });
        });
    }

    fn stop_selected(&mut self, force: bool) {
        let Some(listener) = self.selected_port().cloned() else {
            return;
        };
        self.set_status(format!("Stopping {} on :{}…", listener.name, listener.port));
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = ports::stop(listener.pid, force).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Stopped { listener, result });
        });
    }

    fn delete(&mut self, paths: Vec<PathBuf>) {
        let jobs: Vec<(PathBuf, u64)> = self
            .junk
            .iter()
            .filter(|j| paths.contains(&j.path))
            .map(|j| (j.path.clone(), j.size))
            .collect();
        for (path, _) in &jobs {
            self.marked.remove(path);
            self.deleting.insert(path.clone());
        }
        let tx = self.tx.clone();
        thread::spawn(move || {
            for (path, size) in jobs {
                let result = clean::delete(&path).map_err(|e| e.to_string());
                let _ = tx.send(Msg::Deleted { path, size, result });
            }
        });
    }

    fn receive(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Ports(result) => {
                    self.refreshing = false;
                    self.refreshed = Some(Instant::now());
                    match result {
                        Ok(ports) => self.ports = ports,
                        Err(err) => self.set_status(err),
                    }
                }
                Msg::Found { scan, junk } if scan == self.scan => {
                    self.junk.push(junk);
                    self.sort_junk();
                }
                Msg::ScanDone { scan } if scan == self.scan => self.scanning = false,
                Msg::Found { .. } | Msg::ScanDone { .. } => {}
                Msg::Stopped { listener, result } => {
                    let name = format!("{} on :{}", listener.name, listener.port);
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
            KeyCode::Tab | KeyCode::BackTab => {
                self.tab = match self.tab {
                    Tab::Ports => Tab::Clean,
                    Tab::Clean => Tab::Ports,
                }
            }
            KeyCode::Char('1') => self.tab = Tab::Ports,
            KeyCode::Char('2') => self.tab = Tab::Clean,
            KeyCode::Down | KeyCode::Char('j') => self.table().select_next(),
            KeyCode::Up | KeyCode::Char('k') => self.table().select_previous(),
            KeyCode::Home | KeyCode::Char('g') => self.table().select_first(),
            KeyCode::End | KeyCode::Char('G') => self.table().select_last(),
            code => match self.tab {
                Tab::Ports => self.on_ports_key(code),
                Tab::Clean => self.on_clean_key(code),
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
        }
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
        }
        self.draw_footer(frame, footer);
        if let Some(paths) = &self.confirm {
            self.draw_confirm(frame, paths);
        }
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let [title, tabs] =
            Layout::horizontal([Constraint::Length(8), Constraint::Min(0)]).areas(area);
        frame.render_widget(Span::from(" tidyup").bold().cyan(), title);
        let selected = match self.tab {
            Tab::Ports => 0,
            Tab::Clean => 1,
        };
        frame.render_widget(
            Tabs::new(["1 Ports", "2 Clean"])
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
            .map(|l| l.name.len())
            .max()
            .unwrap_or(0)
            .clamp(7, 24);
        let rows: Vec<Row> = visible
            .iter()
            .map(|l| {
                Row::new([
                    Cell::from(l.port.to_string()).style(Style::new().cyan().bold()),
                    Cell::from(l.name.clone()),
                    Cell::from(l.pid.to_string()),
                    Cell::from(l.uptime.map(fmt::duration).unwrap_or_default()),
                    Cell::from(l.memory.map(fmt::size).unwrap_or_default()),
                    Cell::from(l.cwd.as_deref().map(fmt::path).unwrap_or_default()),
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
                ("r", "rescan"),
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
