//! Ratatui TUI: unified node table, sparklines, page-map, vim-motion navigation.
//!
//! # Layout
//!
//! ```text
//! ┌─ Nodes ───────────────────────────────────────────────────────────────────────┐
//! │  #  │ Type  │ Address         │ Status │ Ring │ Own │ Writes │ Reads │ Recs  │
//! │  0  │ Quick │ 127.0.0.1:7700  │ OK     │  3   │  1  │  1234  │   56  │  —    │
//! │> 1  │ Slow  │ 127.0.0.1:7800  │ OK     │  —   │  —  │   —    │   —   │ 5000  │
//! └────────────────────────────────────────────────────────────────────────────────┘
//! ┌─ Write IOps ──────────────────┐ ┌─ Read IOps ────────────────────────────────┐
//! │ ▁▂▃▄▅▆▇█▇▆▅▄▃▂▁              │ │ ▁▂▃▄▅▆▇█▇▆▅▄▃▂▁                          │
//! └────────────────────────────────┘ └────────────────────────────────────────────┘
//! ┌─ Events ───────────────────────────────────────────────────────────────────────┐
//! │  stress-test log lines …                                                       │
//! └─────────────────────────────────────────────────────────────────────────────────┘
//! ┌─ Page Map — node[0] Quick — 204800 B — 50 pages ──────────────────────────────┐
//! │  ████░░░░░░                                                                    │
//! └────────────────────────────────────────────────────────────────────────────────┘
//! ┌─ System ────────────────────────────────────────────────────────────────────────┐
//! │  Process RSS: 42 MB  │  CPU: 1.2 %  │  Node est. mem: 64 KB                   │
//! └─────────────────────────────────────────────────────────────────────────────────┘
//! [j/k] move  [g/G] top/bottom  [/] search  [n/N] next/prev  [q] quit
//! ```

use std::collections::VecDeque;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::bar::NINE_LEVELS;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Sparkline, Table, TableState};

use crate::poll::{ClusterSnapshot, NodeEntry};

// ── Constants ─────────────────────────────────────────────────────────────────

const SPARKLINE_LEN: usize = 60;

// ── AppState ──────────────────────────────────────────────────────────────────

pub struct AppState {
    pub snapshot: ClusterSnapshot,
    pub table: TableState,
    search_query: String,
    search_matches: Vec<usize>,
    search_cursor: usize,
    pub mode: InputMode,
    pub write_history: VecDeque<u64>,
    pub read_history: VecDeque<u64>,
    prev_writes: u64,
    prev_reads: u64,
    pub process_rss: u64,
    pub process_cpu: f32,
    pub event_log: VecDeque<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    Search,
}

impl AppState {
    pub fn new() -> Self {
        let mut table = TableState::default();
        table.select(Some(0));
        Self {
            snapshot: ClusterSnapshot::default(),
            table,
            search_query: String::new(),
            search_matches: Vec::new(),
            search_cursor: 0,
            mode: InputMode::Normal,
            write_history: VecDeque::from(vec![0; SPARKLINE_LEN]),
            read_history: VecDeque::from(vec![0; SPARKLINE_LEN]),
            prev_writes: 0,
            prev_reads: 0,
            process_rss: 0,
            process_cpu: 0.0,
            event_log: VecDeque::new(),
        }
    }

    pub fn push_log(&mut self, msg: impl Into<String>) {
        const MAX_LOG: usize = 500;
        if self.event_log.len() >= MAX_LOG {
            self.event_log.pop_front();
        }
        self.event_log.push_back(msg.into());
    }

    pub fn update(&mut self, snapshot: ClusterSnapshot) {
        self.snapshot = snapshot;

        // Sparklines: sum writes/reads from Quick nodes only.
        let total_writes: u64 = self
            .snapshot
            .nodes
            .iter()
            .filter_map(|n| {
                if let NodeEntry::Quick { metrics: Some(m), .. } = n {
                    Some(m.write_count)
                } else {
                    None
                }
            })
            .sum();
        let total_reads: u64 = self
            .snapshot
            .nodes
            .iter()
            .filter_map(|n| {
                if let NodeEntry::Quick { metrics: Some(m), .. } = n {
                    Some(m.read_count)
                } else {
                    None
                }
            })
            .sum();

        let write_delta = total_writes.saturating_sub(self.prev_writes);
        let read_delta = total_reads.saturating_sub(self.prev_reads);
        self.prev_writes = total_writes;
        self.prev_reads = total_reads;

        if self.write_history.len() >= SPARKLINE_LEN {
            self.write_history.pop_front();
        }
        if self.read_history.len() >= SPARKLINE_LEN {
            self.read_history.pop_front();
        }
        self.write_history.push_back(write_delta);
        self.read_history.push_back(read_delta);

        self.refresh_search();

        let n = self.snapshot.nodes.len();
        if n == 0 {
            self.table.select(None);
        } else if let Some(sel) = self.table.selected() {
            if sel >= n {
                self.table.select(Some(n - 1));
            }
        } else {
            self.table.select(Some(0));
        }
    }

    // ── Vim motions ───────────────────────────────────────────────────────────

    pub fn move_down(&mut self) {
        let n = self.snapshot.nodes.len();
        if n == 0 {
            return;
        }
        let next = self.table.selected().map_or(0, |i| (i + 1).min(n - 1));
        self.table.select(Some(next));
    }

    pub fn move_up(&mut self) {
        let n = self.snapshot.nodes.len();
        if n == 0 {
            return;
        }
        let prev = self.table.selected().map_or(0, |i| i.saturating_sub(1));
        self.table.select(Some(prev));
    }

    pub fn go_top(&mut self) {
        if !self.snapshot.nodes.is_empty() {
            self.table.select(Some(0));
        }
    }

    pub fn go_bottom(&mut self) {
        let n = self.snapshot.nodes.len();
        if n > 0 {
            self.table.select(Some(n - 1));
        }
    }

    // ── Search ────────────────────────────────────────────────────────────────

    pub fn search_push(&mut self, c: char) {
        self.search_query.push(c);
        self.refresh_search();
        self.jump_to_first_match();
    }

    pub fn search_pop(&mut self) {
        self.search_query.pop();
        self.refresh_search();
        self.jump_to_first_match();
    }

    pub fn search_next(&mut self) {
        if self.search_matches.is_empty() {
            return;
        }
        self.search_cursor = (self.search_cursor + 1) % self.search_matches.len();
        let idx = self.search_matches[self.search_cursor];
        self.table.select(Some(idx));
    }

    pub fn search_prev(&mut self) {
        if self.search_matches.is_empty() {
            return;
        }
        self.search_cursor = self
            .search_cursor
            .checked_sub(1)
            .unwrap_or(self.search_matches.len() - 1);
        let idx = self.search_matches[self.search_cursor];
        self.table.select(Some(idx));
    }

    pub fn clear_search(&mut self) {
        self.search_query.clear();
        self.search_matches.clear();
        self.search_cursor = 0;
    }

    fn refresh_search(&mut self) {
        if self.search_query.is_empty() {
            self.search_matches.clear();
            return;
        }
        let q = self.search_query.to_lowercase();
        self.search_matches = self
            .snapshot
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| {
                let mut hay = n.url().to_lowercase();
                if let NodeEntry::Quick { metrics: Some(m), .. } = n {
                    hay.push_str(&format!(" {:016x}", m.node_id));
                }
                hay.contains(&q)
            })
            .map(|(i, _)| i)
            .collect();
    }

    fn jump_to_first_match(&mut self) {
        if let Some(&idx) = self.search_matches.first() {
            self.search_cursor = 0;
            self.table.select(Some(idx));
        }
    }

    pub fn is_match(&self, node_idx: usize) -> bool {
        self.search_matches.contains(&node_idx)
    }
}

// ── Key handling ──────────────────────────────────────────────────────────────

pub fn handle_key(state: &mut AppState, key: KeyEvent) -> bool {
    match state.mode {
        InputMode::Normal => handle_normal(state, key),
        InputMode::Search => handle_search(state, key),
    }
}

fn handle_normal(state: &mut AppState, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return false,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return false,
        KeyCode::Char('j') | KeyCode::Down => state.move_down(),
        KeyCode::Char('k') | KeyCode::Up => state.move_up(),
        KeyCode::Char('g') => state.go_top(),
        KeyCode::Char('G') => state.go_bottom(),
        KeyCode::Char('n') => state.search_next(),
        KeyCode::Char('N') => state.search_prev(),
        KeyCode::Char('/') => {
            state.mode = InputMode::Search;
            state.clear_search();
        }
        _ => {}
    }
    true
}

fn handle_search(state: &mut AppState, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Esc => {
            state.mode = InputMode::Normal;
            state.clear_search();
        }
        KeyCode::Enter => {
            state.mode = InputMode::Normal;
        }
        KeyCode::Backspace => state.search_pop(),
        KeyCode::Char(c) => state.search_push(c),
        _ => {}
    }
    true
}

// ── Render ────────────────────────────────────────────────────────────────────

pub fn render(f: &mut Frame, state: &mut AppState) {
    let area = f.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),    // unified node table
            Constraint::Length(3), // sparklines
            Constraint::Length(4), // events log
            Constraint::Length(8), // page map
            Constraint::Length(3), // system
            Constraint::Length(1), // status bar
        ])
        .split(area);

    render_nodes_table(f, state, chunks[0]);
    render_sparklines(f, state, chunks[1]);
    render_event_log(f, state, chunks[2]);
    render_page_map(f, state, chunks[3]);
    render_system(f, state, chunks[4]);
    render_statusbar(f, state, chunks[5]);
}

// ── Node table ────────────────────────────────────────────────────────────────

fn render_nodes_table(f: &mut Frame, state: &mut AppState, area: Rect) {
    let header = Row::new(vec![
        Cell::from(" # "),
        Cell::from("Type"),
        Cell::from("Address"),
        Cell::from("Status"),
        Cell::from("Ring"),
        Cell::from("Own"),
        Cell::from("Writes"),
        Cell::from("Reads"),
        Cell::from("Records"),
        Cell::from("Flush"),
        Cell::from("Uptime"),
    ])
    .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = state
        .snapshot
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| node_row(i, n, state.is_match(i)))
        .collect();

    let widths = [
        Constraint::Length(4),  // #
        Constraint::Length(6),  // Type
        Constraint::Length(22), // Address
        Constraint::Length(7),  // Status
        Constraint::Length(5),  // Ring
        Constraint::Length(4),  // Own
        Constraint::Length(8),  // Writes
        Constraint::Length(8),  // Reads
        Constraint::Length(8),  // Records
        Constraint::Length(6),  // Flush
        Constraint::Length(8),  // Uptime
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" Nodes "))
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );

    f.render_stateful_widget(table, area, &mut state.table);
}

fn d() -> Cell<'static> {
    Cell::from("—")
}

fn node_row(idx: usize, n: &NodeEntry, is_match: bool) -> Row<'_> {
    let match_style = if is_match {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };

    match n {
        NodeEntry::Quick { url, metrics, error } => {
            let type_cell =
                Cell::from("Quick").style(Style::default().fg(Color::LightGreen));
            match metrics {
                Some(m) => {
                    let status = if m.is_draining {
                        Cell::from("DRAIN").style(Style::default().fg(Color::Red))
                    } else {
                        Cell::from("OK").style(Style::default().fg(Color::Green))
                    };
                    Row::new(vec![
                        Cell::from(format!(" {idx} ")).style(match_style),
                        type_cell,
                        Cell::from(m.listen_addr.clone()).style(match_style),
                        status,
                        Cell::from(m.ring_size.to_string()),
                        Cell::from(m.owned_partitions.to_string()),
                        Cell::from(m.write_count.to_string()),
                        Cell::from(m.read_count.to_string()),
                        d(),
                        d(),
                        Cell::from(format!("{}s", m.uptime_secs)),
                    ])
                }
                None => Row::new(vec![
                    Cell::from(format!(" {idx} ")).style(match_style),
                    type_cell,
                    Cell::from(url.clone()).style(Style::default().fg(Color::Red)),
                    Cell::from(if *error { "ERR" } else { "—" })
                        .style(Style::default().fg(Color::Red)),
                    d(), d(), d(), d(), d(), d(), d(),
                ]),
            }
        }
        NodeEntry::Slow { url, metrics, error } => {
            let type_cell =
                Cell::from("Slow").style(Style::default().fg(Color::LightBlue));
            match metrics {
                Some(m) => {
                    Row::new(vec![
                        Cell::from(format!(" {idx} ")).style(match_style),
                        type_cell,
                        Cell::from(url.clone()).style(match_style),
                        Cell::from("OK").style(Style::default().fg(Color::Green)),
                        d(),
                        d(),
                        d(),
                        d(),
                        Cell::from(m.record_count.to_string()),
                        Cell::from(format!("{}", m.flush_count)),
                        Cell::from(format!("{}s", m.uptime_secs)),
                    ])
                }
                None => Row::new(vec![
                    Cell::from(format!(" {idx} ")).style(match_style),
                    type_cell,
                    Cell::from(url.clone()).style(Style::default().fg(Color::Red)),
                    Cell::from(if *error { "ERR" } else { "—" })
                        .style(Style::default().fg(Color::Red)),
                    d(), d(), d(), d(), d(), d(), d(),
                ]),
            }
        }
    }
}

// ── Sparklines ────────────────────────────────────────────────────────────────

fn render_sparklines(f: &mut Frame, state: &AppState, area: Rect) {
    let halves = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let write_data: Vec<u64> = state.write_history.iter().copied().collect();
    let read_data: Vec<u64> = state.read_history.iter().copied().collect();

    let write_spark = Sparkline::default()
        .block(Block::default().borders(Borders::ALL).title(" Write IOps "))
        .data(&write_data)
        .style(Style::default().fg(Color::Green))
        .bar_set(NINE_LEVELS);

    let read_spark = Sparkline::default()
        .block(Block::default().borders(Borders::ALL).title(" Read IOps "))
        .data(&read_data)
        .style(Style::default().fg(Color::Blue))
        .bar_set(NINE_LEVELS);

    f.render_widget(write_spark, halves[0]);
    f.render_widget(read_spark, halves[1]);
}

// ── Page map ──────────────────────────────────────────────────────────────────

fn render_page_map(f: &mut Frame, state: &AppState, area: Rect) {
    let selected_entry = state
        .table
        .selected()
        .and_then(|i| state.snapshot.nodes.get(i));

    let title = match selected_entry {
        Some(NodeEntry::Quick { metrics: Some(m), .. }) => {
            let i = state.table.selected().unwrap_or(0);
            format!(
                " Page Map — node[{i}] Quick — {} B — {} pages ",
                m.write_bytes, m.page_count
            )
        }
        Some(NodeEntry::Quick { .. }) => {
            let i = state.table.selected().unwrap_or(0);
            format!(" Page Map — node[{i}] Quick — no data ")
        }
        Some(NodeEntry::Slow { .. }) => " Page Map — Slow nodes have no page map ".to_string(),
        None => " Page Map — (no node selected) ".to_string(),
    };

    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let page_map = match selected_entry {
        Some(NodeEntry::Quick { metrics: Some(m), .. }) => &m.page_map,
        _ => return,
    };

    if inner.height == 0 || inner.width == 0 || page_map.is_empty() {
        return;
    }

    let cols = inner.width as usize;
    let rows = inner.height as usize;
    let lines: Vec<Line> = page_map
        .chunks(cols)
        .take(rows)
        .map(|row_pages| {
            let spans: Vec<Span> = row_pages
                .iter()
                .map(|&occ| {
                    let (ch, color) = match occ {
                        0 => ('·', Color::DarkGray),
                        1..=63 => ('░', Color::Green),
                        64..=127 => ('▒', Color::Yellow),
                        128..=191 => ('▓', Color::LightYellow),
                        _ => ('█', Color::Red),
                    };
                    Span::styled(ch.to_string(), Style::default().fg(color))
                })
                .collect();
            Line::from(spans)
        })
        .collect();

    f.render_widget(Paragraph::new(lines), inner);
}

// ── System ────────────────────────────────────────────────────────────────────

fn render_system(f: &mut Frame, state: &AppState, area: Rect) {
    let rss_mb = state.process_rss / (1024 * 1024);

    let node_mem_span = state
        .table
        .selected()
        .and_then(|i| state.snapshot.nodes.get(i))
        .and_then(|n| {
            if let NodeEntry::Quick { metrics: Some(m), .. } = n {
                Some(m.estimated_memory_bytes / 1024)
            } else {
                None
            }
        })
        .map(|kb| {
            vec![
                Span::raw("  │  Node est. mem: "),
                Span::styled(format!("{kb} KB"), Style::default().fg(Color::Magenta)),
            ]
        })
        .unwrap_or_default();

    let mut spans = vec![
        Span::raw("  Process RSS: "),
        Span::styled(format!("{rss_mb} MB"), Style::default().fg(Color::Magenta)),
        Span::raw("  CPU: "),
        Span::styled(
            format!("{:.1} %", state.process_cpu),
            Style::default().fg(Color::Magenta),
        ),
    ];
    spans.extend(node_mem_span);

    let p = Paragraph::new(Line::from(spans))
        .block(Block::default().borders(Borders::ALL).title(" System "));
    f.render_widget(p, area);
}

// ── Event log ─────────────────────────────────────────────────────────────────

fn render_event_log(f: &mut Frame, state: &AppState, area: Rect) {
    let inner_height = area.height.saturating_sub(2) as usize;
    let total = state.event_log.len();
    let skip = total.saturating_sub(inner_height);

    let lines: Vec<Line> = state
        .event_log
        .iter()
        .skip(skip)
        .map(|msg| Line::from(Span::raw(format!("  {msg}"))))
        .collect();

    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" Events "));
    f.render_widget(p, area);
}

// ── Status bar ────────────────────────────────────────────────────────────────

fn render_statusbar(f: &mut Frame, state: &AppState, area: Rect) {
    let text = match state.mode {
        InputMode::Normal => {
            let hint = if state.search_matches.is_empty() && !state.search_query.is_empty() {
                format!("  /{} (no match)", state.search_query)
            } else if !state.search_query.is_empty() {
                format!(
                    "  /{} ({}/{} match)",
                    state.search_query,
                    state.search_cursor + 1,
                    state.search_matches.len()
                )
            } else {
                "  [j/k] move  [g/G] top/bot  [/] search  [n/N] next/prev  [q] quit"
                    .to_string()
            };
            Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray)))
        }
        InputMode::Search => Line::from(vec![
            Span::styled("SEARCH: /", Style::default().fg(Color::Yellow)),
            Span::styled(&state.search_query, Style::default().fg(Color::White)),
            Span::styled(
                "  [Enter] confirm  [Esc] cancel",
                Style::default().fg(Color::DarkGray),
            ),
        ]),
    };
    f.render_widget(Paragraph::new(text), area);
}

// ── Async event helper ────────────────────────────────────────────────────────

pub fn poll_key() -> Option<KeyEvent> {
    if event::poll(std::time::Duration::ZERO).ok()? {
        if let Event::Key(k) = event::read().ok()? {
            return Some(k);
        }
    }
    None
}
