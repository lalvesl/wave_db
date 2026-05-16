//! Ratatui TUI: node table, sparklines, slow-node block, vim-motion navigation.
//!
//! # Layout
//!
//! ```text
//! ┌─ Quick-Nodes ─────────────────────────────────────────────────────────────┐
//! │  #  │ Node ID  │ Address         │ Status  │ Ring │ Own │ Writes │ Reads  │
//! │  0  │ deadbeef │ 127.0.0.1:7700  │ OK      │  3   │  1 │  1234  │   56   │
//! │> 1  │ cafebabe │ 127.0.0.1:7701  │ DRAIN   │  3   │  1 │   888  │   11   │
//! └────────────────────────────────────────────────────────────────────────────┘
//! ┌─ Write IOps ──────────────────┐ ┌─ Read IOps ────────────────────────────┐
//! │ ▁▂▃▄▅▆▇█▇▆▅▄▃▂▁▂▃▄▅▆▇█▇▆▅▄▃ │ │ ▁▂▃▄▅▆▇█▇▆▅▄▃▂▁▂▃▄▅▆▇█▇▆▅▄▃▂▁▂▃▄▅▆▇█ │
//! └────────────────────────────────┘ └────────────────────────────────────────┘
//! ┌─ Slow-Node ─────────────────────────────────────────────────────────────┐
//! │  Records: 5000  │  Tenants: 3  │  Flushes: 100  │  Uptime: 42s        │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ┌─ System ────────────────────────────────────────────────────────────────┐
//! │  Process RSS: 42 MB  │  CPU: 1.2 %                                     │
//! └─────────────────────────────────────────────────────────────────────────┘
//! [j/k] move  [g/G] top/bottom  [/] search  [n/N] next/prev  [q] quit
//! ```

use std::collections::VecDeque;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::bar::NINE_LEVELS;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Paragraph, Row, Sparkline, Table, TableState,
};

use crate::poll::{ClusterSnapshot, NodeSnapshot};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Number of sparkline data points retained (ring buffer length).
const SPARKLINE_LEN: usize = 60;

// ── AppState ──────────────────────────────────────────────────────────────────

pub struct AppState {
    /// Latest cluster snapshot.
    pub snapshot: ClusterSnapshot,
    /// Table selection.
    pub table: TableState,
    /// Search string (built while in search mode).
    search_query: String,
    /// Indices of nodes whose address/id matches the current search.
    search_matches: Vec<usize>,
    /// Which match is currently highlighted.
    search_cursor: usize,
    pub mode: InputMode,
    /// Historical total-write-count per poll tick for sparklines.
    pub write_history: VecDeque<u64>,
    /// Historical total-read-count per poll tick for sparklines.
    pub read_history: VecDeque<u64>,
    /// Previous write total (to compute delta).
    prev_writes: u64,
    /// Previous read total (to compute delta).
    prev_reads: u64,
    /// Process RSS in bytes (filled by sysinfo each tick).
    pub process_rss: u64,
    /// Process CPU % (filled by sysinfo each tick).
    pub process_cpu: f32,
    /// Log messages from the embedding application (e.g. stress-test events).
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

    /// Append a message to the event log (shown in the Events panel).
    pub fn push_log(&mut self, msg: impl Into<String>) {
        const MAX_LOG: usize = 500;
        if self.event_log.len() >= MAX_LOG {
            self.event_log.pop_front();
        }
        self.event_log.push_back(msg.into());
    }

    /// Ingest a fresh snapshot and update sparklines.
    pub fn update(&mut self, snapshot: ClusterSnapshot) {
        self.snapshot = snapshot;

        // Sum writes and reads across all healthy quick-nodes.
        let total_writes: u64 = self
            .snapshot
            .quick
            .iter()
            .filter_map(|n| n.metrics.as_ref())
            .map(|m| m.write_count)
            .sum();
        let total_reads: u64 = self
            .snapshot
            .quick
            .iter()
            .filter_map(|n| n.metrics.as_ref())
            .map(|m| m.read_count)
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

        // Re-run search so matches stay in sync after topology changes.
        self.refresh_search();

        // Clamp table selection to valid range.
        let n = self.snapshot.quick.len();
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

    // ── Vim motions ───────────────────────────────────────────────────────

    pub fn move_down(&mut self) {
        let n = self.snapshot.quick.len();
        if n == 0 {
            return;
        }
        let next = self.table.selected().map_or(0, |i| (i + 1).min(n - 1));
        self.table.select(Some(next));
    }

    pub fn move_up(&mut self) {
        let n = self.snapshot.quick.len();
        if n == 0 {
            return;
        }
        let prev = self.table.selected().map_or(0, |i| i.saturating_sub(1));
        self.table.select(Some(prev));
    }

    pub fn go_top(&mut self) {
        if !self.snapshot.quick.is_empty() {
            self.table.select(Some(0));
        }
    }

    pub fn go_bottom(&mut self) {
        let n = self.snapshot.quick.len();
        if n > 0 {
            self.table.select(Some(n - 1));
        }
    }

    // ── Search ────────────────────────────────────────────────────────────

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
            .quick
            .iter()
            .enumerate()
            .filter(|(_, n)| {
                let hay = format!(
                    "{} {}",
                    n.url,
                    n.metrics
                        .as_ref()
                        .map_or(String::new(), |m| format!("{:016x}", m.node_id))
                )
                .to_lowercase();
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

    /// `true` if `node_idx` is a search match.
    pub fn is_match(&self, node_idx: usize) -> bool {
        self.search_matches.contains(&node_idx)
    }
}

// ── Key handling ──────────────────────────────────────────────────────────────

/// Process one terminal key event. Returns `false` when the user wants to quit.
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
            Constraint::Min(5),     // quick-node table
            Constraint::Length(3),  // sparklines
            Constraint::Length(4),  // info row: slow-node | events
            Constraint::Length(8),  // page map for selected node
            Constraint::Length(3),  // system block
            Constraint::Length(1),  // status bar
        ])
        .split(area);

    render_quick_table(f, state, chunks[0]);
    render_sparklines(f, state, chunks[1]);

    // Info row: slow-node metrics on the left, event log on the right.
    let info = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(chunks[2]);
    render_slow_node(f, state, info[0]);
    render_event_log(f, state, info[1]);

    render_page_map(f, state, chunks[3]);
    render_system(f, state, chunks[4]);
    render_statusbar(f, state, chunks[5]);
}

fn render_quick_table(f: &mut Frame, state: &mut AppState, area: Rect) {
    let header = Row::new(vec![
        Cell::from(" # "),
        Cell::from("Node ID"),
        Cell::from("Address"),
        Cell::from("Status"),
        Cell::from("Ring"),
        Cell::from("Own"),
        Cell::from("Writes"),
        Cell::from("Reads"),
        Cell::from("Uptime"),
    ])
    .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = state
        .snapshot
        .quick
        .iter()
        .enumerate()
        .map(|(i, n)| node_row(i, n, state.is_match(i)))
        .collect();

    let widths = [
        Constraint::Length(4),
        Constraint::Length(18),
        Constraint::Length(22),
        Constraint::Length(7),
        Constraint::Length(5),
        Constraint::Length(4),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(8),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" Quick-Nodes "))
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );

    f.render_stateful_widget(table, area, &mut state.table);
}

fn node_row<'a>(idx: usize, n: &'a NodeSnapshot, is_match: bool) -> Row<'a> {
    let match_style = if is_match {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };

    match &n.metrics {
        Some(m) => {
            let status = if m.is_draining {
                Cell::from("DRAIN").style(Style::default().fg(Color::Red))
            } else {
                Cell::from("OK").style(Style::default().fg(Color::Green))
            };
            Row::new(vec![
                Cell::from(format!(" {idx} ")).style(match_style),
                Cell::from(format!("{:016x}", m.node_id)).style(match_style),
                Cell::from(m.listen_addr.clone()).style(match_style),
                status,
                Cell::from(m.ring_size.to_string()),
                Cell::from(m.owned_partitions.to_string()),
                Cell::from(m.write_count.to_string()),
                Cell::from(m.read_count.to_string()),
                Cell::from(format!("{}s", m.uptime_secs)),
            ])
        }
        None => {
            let err_style = Style::default().fg(Color::Red);
            Row::new(vec![
                Cell::from(format!(" {idx} ")).style(match_style),
                Cell::from("—").style(err_style),
                Cell::from(n.url.clone()).style(err_style),
                Cell::from(if n.error { "ERR" } else { "—" }).style(err_style),
                Cell::from("—"),
                Cell::from("—"),
                Cell::from("—"),
                Cell::from("—"),
                Cell::from("—"),
            ])
        }
    }
}

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

fn render_slow_node(f: &mut Frame, state: &AppState, area: Rect) {
    let lines = match &state.snapshot.slow {
        Some(m) => {
            let journal_kb = m.journal_estimated_bytes / 1024;
            vec![
                Line::from(vec![
                    Span::raw("  Records: "),
                    Span::styled(m.record_count.to_string(), Style::default().fg(Color::Cyan)),
                    Span::raw("  Tenants: "),
                    Span::styled(m.tenant_count.to_string(), Style::default().fg(Color::Cyan)),
                ]),
                Line::from(vec![
                    Span::raw("  Flushes: "),
                    Span::styled(m.flush_count.to_string(), Style::default().fg(Color::Cyan)),
                    Span::raw("  Journal: "),
                    Span::styled(
                        format!("{journal_kb} KB"),
                        Style::default().fg(Color::Cyan),
                    ),
                ]),
            ]
        }
        None => vec![Line::from(Span::styled(
            "  Slow-node unreachable",
            Style::default().fg(Color::Red),
        ))],
    };

    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" Slow-Node "));
    f.render_widget(p, area);
}

fn render_page_map(f: &mut Frame, state: &AppState, area: Rect) {
    let selected = state.table.selected();
    let node = selected.and_then(|i| state.snapshot.quick.get(i));
    let metrics = node.and_then(|n| n.metrics.as_ref());

    let title = match (selected, metrics) {
        (Some(i), Some(m)) => format!(
            " Page Map — node[{i}] — {} B written — {} pages ",
            m.write_bytes, m.page_count
        ),
        (Some(i), None) => format!(" Page Map — node[{i}] — no data "),
        _ => " Page Map — (no node selected) ".to_string(),
    };

    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if let Some(m) = metrics {
        if inner.height == 0 || inner.width == 0 || m.page_map.is_empty() {
            return;
        }
        let cols = inner.width as usize;
        let rows = inner.height as usize;
        let max_pages = cols * rows;
        let pages: Vec<u8> = m.page_map.iter().take(max_pages).copied().collect();

        let lines: Vec<Line> = pages
            .chunks(cols)
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

        let p = Paragraph::new(lines);
        f.render_widget(p, inner);
    }
}

fn render_system(f: &mut Frame, state: &AppState, area: Rect) {
    let rss_mb = state.process_rss / (1024 * 1024);

    let node_mem_span = state
        .table
        .selected()
        .and_then(|i| state.snapshot.quick.get(i))
        .and_then(|n| n.metrics.as_ref())
        .map(|m| {
            let kb = m.estimated_memory_bytes / 1024;
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
    let p = Paragraph::new(text);
    f.render_widget(p, area);
}

// ── Async event helper ────────────────────────────────────────────────────────

/// Poll crossterm for a key event with a zero timeout.
///
/// Returns `Some(KeyEvent)` if one is available, `None` otherwise.
pub fn poll_key() -> Option<KeyEvent> {
    if event::poll(std::time::Duration::ZERO).ok()? {
        if let Event::Key(k) = event::read().ok()? {
            return Some(k);
        }
    }
    None
}
