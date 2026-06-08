//! Ratatui TUI: unified node table, sparklines, page-map, vim-motion navigation.
//!
//! # Layout (normal mode)
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
//! [j/k] move  [g/G] top/bot  [Enter] detail  [[] []] scroll log  [/] search  [q] quit
//! ```

use std::collections::VecDeque;
use std::fmt::Write as _;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::bar::NINE_LEVELS;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Paragraph, Row, Sparkline, Table, TableState,
};

use crate::poll::{ClusterSnapshot, NodeEntry};

// ── Constants ─────────────────────────────────────────────────────────────────

const SPARKLINE_LEN: usize = 60;
const EVENT_SCROLL_STEP: usize = 5;

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
    /// Lines from the bottom of the event log currently scrolled past.
    /// 0 = latest messages visible; N = scrolled N lines up.
    pub event_log_offset: usize,
    /// Node index currently shown in detail view (set when entering NodeDetail mode).
    pub detail_node_idx: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    Search,
    /// Full-screen detail panel for the selected node.
    NodeDetail,
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
            event_log_offset: 0,
            detail_node_idx: 0,
        }
    }

    pub fn push_log(&mut self, msg: impl Into<String>) {
        const MAX_LOG: usize = 500;
        if self.event_log.len() >= MAX_LOG {
            self.event_log.pop_front();
        }
        self.event_log.push_back(msg.into());
        // Auto-scroll to bottom on new messages (only when offset is 0).
    }

    pub fn update(&mut self, snapshot: ClusterSnapshot) {
        self.snapshot = snapshot;

        let total_writes: u64 = self
            .snapshot
            .nodes
            .iter()
            .filter_map(|n| {
                if let NodeEntry::Quick {
                    metrics: Some(m), ..
                } = n
                {
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
                if let NodeEntry::Quick {
                    metrics: Some(m), ..
                } = n
                {
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

    // ── Event log scroll ──────────────────────────────────────────────────────

    pub fn scroll_events_up(&mut self, visible_rows: usize) {
        let max_offset = self.event_log.len().saturating_sub(visible_rows);
        self.event_log_offset =
            (self.event_log_offset + EVENT_SCROLL_STEP).min(max_offset);
    }

    pub fn scroll_events_down(&mut self) {
        self.event_log_offset =
            self.event_log_offset.saturating_sub(EVENT_SCROLL_STEP);
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
        self.search_cursor =
            (self.search_cursor + 1) % self.search_matches.len();
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
                if let NodeEntry::Quick {
                    metrics: Some(m), ..
                } = n
                {
                    let _ = write!(hay, " {:016x}", m.node_id);
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

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

// ── Key handling ──────────────────────────────────────────────────────────────

pub fn handle_key(state: &mut AppState, key: KeyEvent) -> bool {
    match state.mode {
        InputMode::Normal => handle_normal(state, key),
        InputMode::Search => handle_search(state, key),
        InputMode::NodeDetail => handle_detail(state, key),
    }
}

fn handle_normal(state: &mut AppState, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return false,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            return false;
        }
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
        // Enter opens full-screen detail for the selected node.
        KeyCode::Enter => {
            if let Some(idx) = state.table.selected() {
                state.detail_node_idx = idx;
                state.mode = InputMode::NodeDetail;
            }
        }
        // [ / ] scroll the event log up / down.
        KeyCode::Char('[') | KeyCode::PageUp => state.scroll_events_up(4),
        KeyCode::Char(']') | KeyCode::PageDown => state.scroll_events_down(),
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

fn handle_detail(state: &mut AppState, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
            state.mode = InputMode::Normal;
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            return false;
        }
        _ => {}
    }
    true
}

// ── Render ────────────────────────────────────────────────────────────────────

pub fn render(f: &mut Frame, state: &mut AppState) {
    if state.mode == InputMode::NodeDetail {
        let idx = state.detail_node_idx;
        render_node_detail(f, state, idx, f.area());
        return;
    }

    let area = f.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),    // unified node table
            Constraint::Length(3), // sparklines
            Constraint::Length(6), // events log (taller for scroll)
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
        Cell::from("Mem"),
        Cell::from("Disk"),
        Cell::from("Uptime"),
    ])
    .style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );

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
        Constraint::Length(7),  // Mem
        Constraint::Length(7),  // Disk
        Constraint::Length(8),  // Uptime
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Nodes  [Enter] detail "),
        )
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

#[allow(clippy::cast_precision_loss)]
fn human_bytes(b: u64) -> String {
    if b >= 1024 * 1024 * 1024 {
        format!("{:.1}G", b as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if b >= 1024 * 1024 {
        format!("{:.1}M", b as f64 / (1024.0 * 1024.0))
    } else if b >= 1024 {
        format!("{:.1}K", b as f64 / 1024.0)
    } else {
        format!("{b}B")
    }
}

fn node_row(idx: usize, n: &NodeEntry, is_match: bool) -> Row<'_> {
    let match_style = if is_match {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };

    match n {
        NodeEntry::Quick {
            url,
            metrics,
            error,
        } => {
            let type_cell = Cell::from("Quick")
                .style(Style::default().fg(Color::LightGreen));
            match metrics {
                Some(m) => {
                    let status = if m.is_draining {
                        Cell::from("DRAIN")
                            .style(Style::default().fg(Color::Red))
                    } else {
                        Cell::from("OK")
                            .style(Style::default().fg(Color::Green))
                    };
                    let disk = if m.has_storage {
                        let total =
                            m.journal_bytes + m.heap_bytes + m.data_file_bytes;
                        Cell::from(human_bytes(total))
                            .style(Style::default().fg(Color::Yellow))
                    } else {
                        Cell::from("RAM")
                            .style(Style::default().fg(Color::DarkGray))
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
                        Cell::from(human_bytes(m.estimated_memory_bytes))
                            .style(Style::default().fg(Color::Cyan)),
                        disk,
                        Cell::from(format!("{}s", m.uptime_secs)),
                    ])
                }
                None => Row::new(vec![
                    Cell::from(format!(" {idx} ")).style(match_style),
                    type_cell,
                    Cell::from(url.clone())
                        .style(Style::default().fg(Color::Red)),
                    Cell::from(if *error { "ERR" } else { "—" })
                        .style(Style::default().fg(Color::Red)),
                    d(),
                    d(),
                    d(),
                    d(),
                    d(),
                    d(),
                    d(),
                    d(),
                    d(),
                ]),
            }
        }
        NodeEntry::Slow {
            url,
            metrics,
            error,
        } => {
            let type_cell =
                Cell::from("Slow").style(Style::default().fg(Color::LightBlue));
            match metrics {
                Some(m) => Row::new(vec![
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
                    Cell::from(human_bytes(m.index_bytes))
                        .style(Style::default().fg(Color::Cyan)),
                    Cell::from(human_bytes(m.journal_bytes))
                        .style(Style::default().fg(Color::Yellow)),
                    Cell::from(format!("{}s", m.uptime_secs)),
                ]),
                None => Row::new(vec![
                    Cell::from(format!(" {idx} ")).style(match_style),
                    type_cell,
                    Cell::from(url.clone())
                        .style(Style::default().fg(Color::Red)),
                    Cell::from(if *error { "ERR" } else { "—" })
                        .style(Style::default().fg(Color::Red)),
                    d(),
                    d(),
                    d(),
                    d(),
                    d(),
                    d(),
                    d(),
                    d(),
                    d(),
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
        Some(NodeEntry::Quick {
            metrics: Some(m), ..
        }) => {
            let i = state.table.selected().unwrap_or(0);
            let mb = m.write_bytes / (1024 * 1024);
            if m.data_file_page_count > 0 {
                format!(
                    " Page Map — node[{i}] Quick — {mb} MB written — data.bin {}/{} pages used ({:.0}%) ",
                    m.data_file_used_pages,
                    m.data_file_page_count,
                    m.data_file_used_pages as f64
                        / m.data_file_page_count as f64
                        * 100.0,
                )
            } else {
                format!(
                    " Page Map — node[{i}] Quick — {mb} MB written — {} write-buf slots ",
                    m.page_count,
                )
            }
        }
        Some(NodeEntry::Quick { .. }) => {
            let i = state.table.selected().unwrap_or(0);
            format!(" Page Map — node[{i}] Quick — no data ")
        }
        Some(NodeEntry::Slow { .. }) => {
            " Page Map — Slow nodes have no page map ".to_string()
        }
        None => " Page Map — (no node selected) ".to_string(),
    };

    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let page_map = match selected_entry {
        Some(NodeEntry::Quick {
            metrics: Some(m), ..
        }) => &m.page_map,
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
            if let NodeEntry::Quick {
                metrics: Some(m), ..
            } = n
            {
                Some(m.estimated_memory_bytes / 1024)
            } else {
                None
            }
        })
        .map(|kb| {
            vec![
                Span::raw("  │  Node est. mem: "),
                Span::styled(
                    format!("{kb} KB"),
                    Style::default().fg(Color::Magenta),
                ),
            ]
        })
        .unwrap_or_default();

    let mut spans = vec![
        Span::raw("  Process RSS: "),
        Span::styled(
            format!("{rss_mb} MB"),
            Style::default().fg(Color::Magenta),
        ),
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

    // event_log_offset > 0 means scrolled up: show older entries.
    let end = total.saturating_sub(state.event_log_offset);
    let start = end.saturating_sub(inner_height);

    let lines: Vec<Line> = state
        .event_log
        .iter()
        .skip(start)
        .take(end.saturating_sub(start))
        .map(|msg| Line::from(Span::raw(format!("  {msg}"))))
        .collect();

    let scroll_hint = if state.event_log_offset > 0 {
        format!(
            " Events  ↑ scrolled {} lines  []] down ",
            state.event_log_offset
        )
    } else {
        " Events  [[/]] scroll ".to_string()
    };

    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(scroll_hint));
    f.render_widget(p, area);
}

// ── Node detail (full-screen) ─────────────────────────────────────────────────

fn render_node_detail(f: &mut Frame, state: &AppState, idx: usize, area: Rect) {
    let node = state.snapshot.nodes.get(idx);

    match node {
        Some(NodeEntry::Quick {
            url,
            metrics,
            error,
        }) => {
            render_quick_detail(f, idx, url, metrics.as_ref(), *error, area);
        }
        Some(NodeEntry::Slow {
            url,
            metrics,
            error,
        }) => {
            render_slow_detail(f, idx, url, metrics.as_ref(), *error, area);
        }
        None => {
            let block = Block::default()
                .borders(Borders::ALL)
                .title(" Node Detail — (no data) ");
            f.render_widget(
                Paragraph::new("  No node data. Press Esc to go back.")
                    .block(block),
                area,
            );
        }
    }
}

#[allow(clippy::cast_precision_loss)]
fn render_quick_detail(
    f: &mut Frame,
    idx: usize,
    url: &str,
    metrics: Option<&wavedb_net::metrics::QuickNodeMetrics>,
    error: bool,
    area: Rect,
) {
    let title = format!(" Quick Node [{idx}] — {url} ");
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(""));

    let Some(m) = metrics else {
        let status = if error {
            "ERR — no metrics available"
        } else {
            "— no data"
        };
        lines.push(Line::from(vec![
            Span::raw("  Status:    "),
            Span::styled(status, Style::default().fg(Color::Red)),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  [Esc] back",
            Style::default().fg(Color::DarkGray),
        )));
        f.render_widget(Paragraph::new(lines), inner);
        return;
    };

    let status_span = if m.is_draining {
        Span::styled("DRAINING", Style::default().fg(Color::Red))
    } else {
        Span::styled("OK", Style::default().fg(Color::Green))
    };

    // Two-column info rows.
    let kv = |label: &'static str, val: String| -> Line<'static> {
        Line::from(vec![
            Span::styled(
                format!("  {label:<18}"),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(val),
        ])
    };

    lines.push(Line::from(vec![
        Span::styled(
            "  Status:           ",
            Style::default().fg(Color::Yellow),
        ),
        status_span,
        Span::styled(
            format!("      Uptime: {}s", m.uptime_secs),
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    lines.push(kv("Ring size:", m.ring_size.to_string()));
    lines.push(kv("Owned partitions:", m.owned_partitions.to_string()));
    lines.push(kv("Writes:", format!("{}", m.write_count)));
    lines.push(kv("Reads:", format!("{}", m.read_count)));
    lines.push(kv("Write bytes:", human_bytes(m.write_bytes)));
    lines.push(kv("Est. memory:", human_bytes(m.estimated_memory_bytes)));
    lines.push(Line::from(""));

    // Storage section.
    lines.push(Line::from(Span::styled(
        "  ── Storage ──────────────────────────────────────────────────────",
        Style::default().fg(Color::DarkGray),
    )));
    if m.has_storage {
        let total = m.journal_bytes + m.heap_bytes + m.data_file_bytes;
        lines.push(kv("Mode:", "On-Disk".to_string()));
        lines.push(Line::from(vec![
            Span::styled(
                "  journal.log        ",
                Style::default().fg(Color::Yellow),
            ),
            Span::styled(
                human_bytes(m.journal_bytes),
                Style::default().fg(Color::White),
            ),
            Span::styled(
                "   WAL — crash-safe durability point",
                Style::default().fg(Color::DarkGray),
            ),
        ]));
        {
            let page_detail = if m.data_file_page_count > 0 {
                format!(
                    "   {}/{} pages used ({:.0}%)",
                    m.data_file_used_pages,
                    m.data_file_page_count,
                    m.data_file_used_pages as f64
                        / m.data_file_page_count as f64
                        * 100.0,
                )
            } else {
                "   Page table — hash-mapped 8 KiB pages".to_string()
            };
            lines.push(Line::from(vec![
                Span::styled(
                    "  data.bin           ",
                    Style::default().fg(Color::Yellow),
                ),
                Span::styled(
                    human_bytes(m.data_file_bytes),
                    Style::default().fg(Color::White),
                ),
                Span::styled(page_detail, Style::default().fg(Color::DarkGray)),
            ]));
        }
        lines.push(Line::from(vec![
            Span::styled(
                "  heap.bin           ",
                Style::default().fg(Color::Yellow),
            ),
            Span::styled(
                human_bytes(m.heap_bytes),
                Style::default().fg(Color::White),
            ),
            Span::styled(
                "   Variable-length blob overflow",
                Style::default().fg(Color::DarkGray),
            ),
        ]));
        lines.push(kv("Total disk:", human_bytes(total)));
    } else {
        lines.push(kv(
            "Mode:",
            "In-Memory (no journal / no disk files)".to_string(),
        ));
    }
    lines.push(Line::from(""));

    // Page map inline (full width).
    lines.push(Line::from(Span::styled(
        "  ── Page Map ─────────────────────────────────────────────────────",
        Style::default().fg(Color::DarkGray),
    )));
    if m.page_map.is_empty() {
        lines.push(Line::from("  (no data yet)"));
    } else {
        let cols = inner.width.saturating_sub(2) as usize;
        for chunk in m.page_map.chunks(cols.max(1)) {
            let spans: Vec<Span> = std::iter::once(Span::raw("  "))
                .chain(chunk.iter().map(|&occ| {
                    let (ch, color) = match occ {
                        0 => ('·', Color::DarkGray),
                        1..=63 => ('░', Color::Green),
                        64..=127 => ('▒', Color::Yellow),
                        128..=191 => ('▓', Color::LightYellow),
                        _ => ('█', Color::Red),
                    };
                    Span::styled(ch.to_string(), Style::default().fg(color))
                }))
                .collect();
            lines.push(Line::from(spans));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  [Esc] / [Enter] — back to cluster view",
        Style::default().fg(Color::DarkGray),
    )));

    f.render_widget(Paragraph::new(lines), inner);
}

fn render_slow_detail(
    f: &mut Frame,
    idx: usize,
    url: &str,
    metrics: Option<&wavedb_net::metrics::SlowNodeMetrics>,
    error: bool,
    area: Rect,
) {
    let title = format!(" Slow Node [{idx}] — {url} ");
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(""));

    let Some(m) = metrics else {
        let status = if error {
            "ERR — no metrics available"
        } else {
            "— no data"
        };
        lines.push(Line::from(vec![
            Span::raw("  Status:    "),
            Span::styled(status, Style::default().fg(Color::Red)),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  [Esc] back",
            Style::default().fg(Color::DarkGray),
        )));
        f.render_widget(Paragraph::new(lines), inner);
        return;
    };

    let kv = |label: &'static str, val: String| -> Line<'static> {
        Line::from(vec![
            Span::styled(
                format!("  {label:<18}"),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(val),
        ])
    };

    lines.push(Line::from(vec![
        Span::styled(
            "  Status:           ",
            Style::default().fg(Color::Yellow),
        ),
        Span::styled("OK", Style::default().fg(Color::Green)),
        Span::styled(
            format!("      Uptime: {}s", m.uptime_secs),
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    lines.push(kv("Records:", m.record_count.to_string()));
    lines.push(kv("Tenants:", m.tenant_count.to_string()));
    lines.push(kv("Flush count:", m.flush_count.to_string()));
    lines.push(Line::from(""));

    lines.push(Line::from(Span::styled(
        "  ── Storage ──────────────────────────────────────────────────────",
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(kv("Mode:", "In-Memory (audit store)".to_string()));
    lines.push(Line::from(vec![
        Span::styled(
            "  Audit journal      ",
            Style::default().fg(Color::Yellow),
        ),
        Span::styled(
            human_bytes(m.journal_bytes),
            Style::default().fg(Color::White),
        ),
        Span::styled(
            "   Append-only versioned history",
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled(
            "  Index              ",
            Style::default().fg(Color::Yellow),
        ),
        Span::styled(
            human_bytes(m.index_bytes),
            Style::default().fg(Color::White),
        ),
        Span::styled(
            "   In-memory record index",
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  [Esc] / [Enter] — back to cluster view",
        Style::default().fg(Color::DarkGray),
    )));

    f.render_widget(Paragraph::new(lines), inner);
}

// ── Status bar ────────────────────────────────────────────────────────────────

fn render_statusbar(f: &mut Frame, state: &AppState, area: Rect) {
    let text = match state.mode {
        InputMode::Normal => {
            let hint = if state.search_matches.is_empty()
                && !state.search_query.is_empty()
            {
                format!("  /{} (no match)", state.search_query)
            } else if !state.search_query.is_empty() {
                format!(
                    "  /{} ({}/{} match)",
                    state.search_query,
                    state.search_cursor + 1,
                    state.search_matches.len()
                )
            } else {
                "  [j/k] move  [g/G] top/bot  [Enter] detail  [[] []] scroll log  [/] search  [q] quit".to_string()
            };
            Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray)))
        }
        InputMode::Search => Line::from(vec![
            Span::styled("SEARCH: /", Style::default().fg(Color::Yellow)),
            Span::styled(
                &state.search_query,
                Style::default().fg(Color::White),
            ),
            Span::styled(
                "  [Enter] confirm  [Esc] cancel",
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        InputMode::NodeDetail => Line::from(Span::styled(
            "  [Esc] / [Enter] — back to cluster view",
            Style::default().fg(Color::DarkGray),
        )),
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
