// `collapsible_if` suggests merging nested `if let` blocks into let-chains.
// We keep the nested form here for readability across the /proc and event-handling code.
#![allow(clippy::collapsible_if)]

mod metrics;
mod docker;

use metrics::{CpuRawState, MemoryMetrics, ProcessMetrics, NetworkMetrics, SystemSnapshot, CoreInfo, LoadAverage, DiskIOState, DiskPartition, fetch_numa_nodes, format_cpu_list, HugePageInfo, InterfaceMetrics, compute_interface_speeds, tcp_retransmits, read_clock_sync};
use docker::{DockerAction, DockerSnapshot};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use bollard::Docker;
use tokio::sync::mpsc;
use tokio::time::sleep;
use futures::StreamExt;

use crossterm::{
    event::{Event, EventStream, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};

use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{
        canvas::{Canvas, Points},
        Block, Borders, Cell, Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState,
        Table, TableState, Tabs,
    },
    Terminal,
};

const HISTORY_LEN: usize = 60;
const DOCKER_LOG_TAIL: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    System,
    Docker,
}

/// Sortable columns for the process table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcSort {
    Pid,
    User,
    Command,
    Cpu,
    Mem,
}

/// Sortable columns for the Docker container table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DockSort {
    Name,
    Image,
    State,
    Cpu,
    Mem,
}

struct AppState {
    active_tab: Tab,
    pending_m: bool,

    // System tab
    cpu_history: VecDeque<f64>,
    mem_history: VecDeque<f64>,
    last_snapshot: Option<SystemSnapshot>,
    process_mode: bool,
    selected_process: usize,
    pending_c: bool,
    proc_sort: ProcSort,
    proc_desc: bool,
    proc_table_state: TableState,
    // process kill feature
    kill_target: Option<(u32, String)>,
    show_kill_confirm: bool,
    show_sudo_prompt: bool,
    sudo_password: String,
    kill_feedback: Option<String>,
    pin_feedback: Option<String>,

    // Docker tab
    docker_snapshot: Option<DockerSnapshot>,
    selected_container: usize,
    dock_sort: DockSort,
    dock_desc: bool,
    dock_table_state: TableState,
    docker_cpu_hist: HashMap<String, VecDeque<f64>>,
    docker_mem_hist: HashMap<String, VecDeque<f64>>,
    docker_confirm: Option<(DockerAction, String, String)>, // (action, id, name)
    docker_feedback: Option<String>,
    show_logs: bool,
    docker_logs: Vec<String>,
    log_scroll: u16,
}

impl AppState {
    fn new() -> Self {
        Self {
            active_tab: Tab::System,
            pending_m: false,
            cpu_history: VecDeque::with_capacity(HISTORY_LEN),
            mem_history: VecDeque::with_capacity(HISTORY_LEN),
            last_snapshot: None,
            process_mode: false,
            selected_process: 0,
            pending_c: false,
            proc_sort: ProcSort::Mem,
            proc_desc: true,
            proc_table_state: TableState::default(),
            kill_target: None,
            show_kill_confirm: false,
            show_sudo_prompt: false,
            sudo_password: String::new(),
            kill_feedback: None,
            pin_feedback: None,
            docker_snapshot: None,
            selected_container: 0,
            dock_sort: DockSort::Name,
            dock_desc: false,
            dock_table_state: TableState::default(),
            docker_cpu_hist: HashMap::new(),
            docker_mem_hist: HashMap::new(),
            docker_confirm: None,
            docker_feedback: None,
            show_logs: false,
            docker_logs: Vec::new(),
            log_scroll: 0,
        }
    }

    fn visible_process_count(&self) -> usize {
        self.last_snapshot.as_ref().map(|s| s.processes.len()).unwrap_or(0)
    }

    fn sort_processes(&mut self) {
        let (key, desc) = (self.proc_sort, self.proc_desc);
        if let Some(ref mut snap) = self.last_snapshot {
            match key {
                ProcSort::Pid => snap.processes.sort_by_key(|p| p.pid),
                ProcSort::User => snap.processes.sort_by_key(|p| p.user.to_lowercase()),
                ProcSort::Command => snap.processes.sort_by_key(|p| p.name.to_lowercase()),
                ProcSort::Cpu => snap.processes.sort_by(|a, b| a.cpu_usage.partial_cmp(&b.cpu_usage).unwrap_or(std::cmp::Ordering::Equal)),
                ProcSort::Mem => snap.processes.sort_by_key(|p| p.mem_res_kb),
            }
            if desc {
                snap.processes.reverse();
            }
        }
    }

    fn sort_containers(&mut self) {
        let (key, desc) = (self.dock_sort, self.dock_desc);
        if let Some(ref mut snap) = self.docker_snapshot {
            match key {
                DockSort::Name => snap.containers.sort_by_key(|c| c.name.to_lowercase()),
                DockSort::Image => snap.containers.sort_by_key(|c| c.image.to_lowercase()),
                DockSort::State => snap.containers.sort_by(|a, b| a.state.cmp(&b.state)),
                DockSort::Cpu => snap.containers.sort_by(|a, b| a.cpu_usage.partial_cmp(&b.cpu_usage).unwrap_or(std::cmp::Ordering::Equal)),
                DockSort::Mem => snap.containers.sort_by(|a, b| a.mem_mb.partial_cmp(&b.mem_mb).unwrap_or(std::cmp::Ordering::Equal)),
            }
            if desc {
                snap.containers.reverse();
            }
        }
    }

    fn container_count(&self) -> usize {
        self.docker_snapshot.as_ref().map(|s| s.containers.len()).unwrap_or(0)
    }

    fn selected_docker(&self) -> Option<&docker::DockerContainer> {
        self.docker_snapshot
            .as_ref()
            .and_then(|s| s.containers.get(self.selected_container))
    }

    fn push_snapshot(&mut self, snapshot: SystemSnapshot) {
        let mem_pct = if snapshot.mem.total_mem_kb > 0 {
            snapshot.mem.used_mem_kb as f64 / snapshot.mem.total_mem_kb as f64 * 100.0
        } else {
            0.0
        };

        if self.cpu_history.len() >= HISTORY_LEN {
            self.cpu_history.pop_front();
        }
        if self.mem_history.len() >= HISTORY_LEN {
            self.mem_history.pop_front();
        }

        self.cpu_history.push_back(snapshot.cpu_usage);
        self.mem_history.push_back(mem_pct);
        self.last_snapshot = Some(snapshot);
        self.sort_processes();
    }

    fn push_docker_snapshot(&mut self, snapshot: DockerSnapshot) {
        // Update per-container history (keyed by id), pruning gone containers.
        let live: HashSet<&str> = snapshot.containers.iter().map(|c| c.id.as_str()).collect();
        self.docker_cpu_hist.retain(|k, _| live.contains(k.as_str()));
        self.docker_mem_hist.retain(|k, _| live.contains(k.as_str()));

        for c in &snapshot.containers {
            let cpu = c.cpu_usage.clamp(0.0, 100.0);
            let mem_pct = if c.mem_limit_mb > 0.0 {
                (c.mem_mb / c.mem_limit_mb * 100.0).clamp(0.0, 100.0)
            } else {
                0.0
            };
            let cpu_hist = self.docker_cpu_hist.entry(c.id.clone()).or_default();
            if cpu_hist.len() >= HISTORY_LEN {
                cpu_hist.pop_front();
            }
            cpu_hist.push_back(cpu);

            let mem_hist = self.docker_mem_hist.entry(c.id.clone()).or_default();
            if mem_hist.len() >= HISTORY_LEN {
                mem_hist.pop_front();
            }
            mem_hist.push_back(mem_pct);
        }

        // Keep selection in range.
        if self.selected_container >= snapshot.containers.len() {
            self.selected_container = snapshot.containers.len().saturating_sub(1);
        }

        self.docker_snapshot = Some(snapshot);
        self.sort_containers();
    }
}

fn render_gradient_chart(
    f: &mut ratatui::Frame,
    data: &std::collections::VecDeque<f64>,
    title: String,
    area: Rect,
) {
    // Magma gradient ordered high→low so the first matching threshold wins
    let gradient: [(f64, Color); 10] = [
        (90.0, Color::Rgb(255, 200,  50)),
        (80.0, Color::Rgb(255, 160,  30)),
        (70.0, Color::Rgb(248, 120,  30)),
        (60.0, Color::Rgb(238,  80,  50)),
        (50.0, Color::Rgb(218,  50,  80)),
        (40.0, Color::Rgb(195,  25, 120)),
        (30.0, Color::Rgb(170,  10, 148)),
        (20.0, Color::Rgb(140,   4, 150)),
        (10.0, Color::Rgb(108,   0, 138)),
        ( 0.0, Color::Rgb( 72,   0, 118)),
    ];

    let n = data.len();
    let hist_f = HISTORY_LEN as f64;

    let mut bands: Vec<Vec<(f64, f64)>> = (0..gradient.len()).map(|_| Vec::new()).collect();
    for (col, &value) in data.iter().enumerate() {
        // Right-align: newest data on the right.
        // Two x sub-samples per bar (at +0.0 and +0.5) guarantee adjacent Braille
        // columns are filled regardless of canvas width, eliminating gaps.
        let x_base = (hist_f - n as f64) + col as f64;
        for &dx in &[0.0_f64, 0.5] {
            let x = x_base + dx;
            let mut y = 0.5f64;
            while y < value {
                let band = gradient
                    .iter()
                    .position(|&(t, _)| y >= t)
                    .unwrap_or(gradient.len() - 1);
                bands[band].push((x, y));
                y += 1.0;
            }
        }
    }

    let canvas = Canvas::default()
        .block(Block::default().title(title).borders(Borders::ALL))
        .x_bounds([0.0, hist_f])
        .y_bounds([0.0, 100.0])
        .marker(symbols::Marker::Braille)
        .paint(move |ctx| {
            for (i, &(_, color)) in gradient.iter().enumerate() {
                if !bands[i].is_empty() {
                    ctx.draw(&Points { coords: &bands[i], color });
                }
            }
        });

    f.render_widget(canvas, area);
}

/// Map a per-core utilization (0-100%) to a heat color + block glyph.
fn core_heat_glyph(usage: f64) -> (Color, &'static str) {
    if usage >= 90.0 { (Color::Red, "█") }
    else if usage >= 70.0 { (Color::Yellow, "▆") }
    else if usage >= 50.0 { (Color::LightRed, "▄") }
    else if usage >= 30.0 { (Color::LightGreen, "▂") }
    else if usage > 0.0 { (Color::Green, "·") }
    else { (Color::DarkGray, "·") }
}

/// Build the rows for a per-core utilization heatmap, 8 cores per row.
/// Each cell is "NN▇" (core number + heat glyph), colored by load.
fn core_heatmap_lines(core_usage: &[f64]) -> Vec<Line<'static>> {
    const PER_ROW: usize = 8;
    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    for (i, &usage) in core_usage.iter().enumerate() {
        if i % PER_ROW == 0 {
            rows.push(Vec::new());
        }
        let (color, glyph) = core_heat_glyph(usage);
        rows.last_mut().expect("row exists").push(Span::styled(
            format!("{:>2}{} ", i, glyph),
            Style::default().fg(color),
        ));
    }
    if rows.is_empty() {
        rows.push(vec![Span::raw("  no cores detected")]);
    }
    rows.into_iter().map(Line::from).collect()
}
/// Write a vertical scrollbar on the right edge of `area` (over the block's
/// right border), with the thumb tracking `pos` within `total` items.
fn render_scrollbar(f: &mut ratatui::Frame, area: Rect, total: usize, pos: usize) {
    if total == 0 || area.height < 3 {
        return;
    }
    // Inset vertically so the arrows don't collide with the block corners.
    let track = Rect {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: area.height.saturating_sub(2),
    };
    let mut sb_state = ScrollbarState::new(total).position(pos);
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(Some("▲"))
        .end_symbol(Some("▼"));
    f.render_stateful_widget(scrollbar, track, &mut sb_state);
}

fn render_tab_bar(f: &mut ratatui::Frame, area: Rect, active: Tab) {
    let idx = match active {
        Tab::System => 0,
        Tab::Docker => 1,
    };
    let tabs = Tabs::new(vec![Line::from(" System "), Line::from(" Docker ")])
        .select(idx)
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .divider("│");
    let hint = " panda — mm: switch tab   q: quit ";
    let bar = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(22), Constraint::Min(0)])
        .split(area);
    f.render_widget(tabs, bar[0]);
    f.render_widget(
        Paragraph::new(hint).style(Style::default().fg(Color::DarkGray)),
        bar[1],
    );
}

fn render_system_tab(f: &mut ratatui::Frame, state: &mut AppState, area: Rect) {
    let snapshot = match state.last_snapshot {
        Some(ref s) => s,
        None => return,
    };
    let mem_pct = if snapshot.mem.total_mem_kb > 0 {
        (snapshot.mem.used_mem_kb as f64 / snapshot.mem.total_mem_kb as f64 * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(40), Constraint::Min(0)].as_ref())
        .split(area);

    let top_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(30), Constraint::Percentage(30)].as_ref())
        .split(chunks[0]);

    let left_sub_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)].as_ref())
        .split(top_chunks[0]);

    let middle_sub_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)].as_ref())
        .split(top_chunks[1]);

    let right_sub_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)].as_ref())
        .split(top_chunks[2]);

    render_gradient_chart(
        f,
        &state.cpu_history,
        format!("CPU  {:.1}%", snapshot.cpu_usage),
        left_sub_chunks[0],
    );

    render_gradient_chart(
        f,
        &state.mem_history,
        format!(
            "MEM  {:.2}GB / {:.2}GB  ({:.1}%)",
            snapshot.mem.used_mem_kb as f64 / 1024.0 / 1024.0,
            snapshot.mem.total_mem_kb as f64 / 1024.0 / 1024.0,
            mem_pct,
        ),
        left_sub_chunks[1],
    );

    let swap_pct = if snapshot.mem.total_swap_kb > 0 {
        snapshot.mem.used_swap_kb as f64 / snapshot.mem.total_swap_kb as f64 * 100.0
    } else {
        0.0
    };
    let mut load_swap_text = format!(
        "\n  Load:  {:.2}  {:.2}  {:.2}\n  Swap:  {:.2}GB / {:.2}GB ({:.1}%)",
        snapshot.load_avg.one,
        snapshot.load_avg.five,
        snapshot.load_avg.fifteen,
        snapshot.mem.used_swap_kb as f64 / 1024.0 / 1024.0,
        snapshot.mem.total_swap_kb as f64 / 1024.0 / 1024.0,
        swap_pct,
    );
    if !snapshot.hugepages.pools.is_empty() {
        let thp = if snapshot.hugepages.thp_enabled.is_empty() {
            String::from("n/a")
        } else {
            snapshot.hugepages.thp_enabled.clone()
        };
        load_swap_text.push_str(&format!("\n  THP:   {}\n", thp));
        for pool in &snapshot.hugepages.pools {
            let total_gb = pool.nr as f64 * pool.size_kb as f64 / 1024.0 / 1024.0 / 1024.0;
            load_swap_text.push_str(&format!(
                "  HP {}K: {}/{} pg free (resv {}, surplus {}), {:.2}GB\n",
                pool.size_kb,
                pool.free,
                pool.nr,
                pool.resv,
                pool.surplus,
                total_gb,
            ));
        }
    }
    f.render_widget(
        Paragraph::new(load_swap_text).block(
            Block::default()
                .title("Load / Swap / HugePages")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        ),
        middle_sub_chunks[0],
    );

    let mut disk_text = format!(
        "\n  Read:  {:.1} KB/s\n  Write: {:.1} KB/s\n",
        snapshot.disk_io.read_kbs, snapshot.disk_io.write_kbs,
    );
    for p in &snapshot.disk_partitions {
        disk_text.push_str(&format!(
            "  Capacity: {:.1}G/{:.0}G ({:.1}%)\n",
            p.used_gb, p.total_gb, p.use_pct,
        ));
    }
    f.render_widget(
        Paragraph::new(disk_text).block(
            Block::default()
                .title("Disk I/O & Capacity")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red)),
        ),
        middle_sub_chunks[1],
    );

    let mut core_lines: Vec<Line> = vec![
        Line::from(format!(
            "  Model:  {}\n  Vendor: {}\n  Cores:  {} ({} logical)",
            snapshot.core_info.cpu_model,
            snapshot.core_info.vendor,
            snapshot.core_info.physical_core,
            snapshot.core_usage.len(),
        )),
        Line::from(""),
    ];
    core_lines.extend(core_heatmap_lines(&snapshot.core_usage));
    if !snapshot.numa_nodes.is_empty() {
        core_lines.push(Line::from(""));
        core_lines.push(Line::from(Span::styled(
            "  NUMA Nodes",
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
        )));
        for node in &snapshot.numa_nodes {
            let pct = if node.mem_total_kb > 0 {
                node.mem_used_kb as f64 / node.mem_total_kb as f64 * 100.0
            } else {
                0.0
            };
            let used_gb = node.mem_used_kb as f64 / 1024.0 / 1024.0 / 1024.0;
            let total_gb = node.mem_total_kb as f64 / 1024.0 / 1024.0 / 1024.0;
            core_lines.push(Line::from(format!(
                "  N{}  {:>4.1}/{:.1}G ({:.0}%)  cpus {}",
                node.id, used_gb, total_gb, pct, format_cpu_list(&node.cpus),
            )));
        }
    }
    f.render_widget(
        Paragraph::new(core_lines).block(
            Block::default()
                .title("CPU Cores / Affinity")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Blue)),
        ),
        right_sub_chunks[0],
    );

    let mut net_text = format!(
        "  ↓ {:.2}  ↑ {:.2} KB/s   retr {}\n",
        snapshot.net_rx_speed, snapshot.net_tx_speed, snapshot.tcp_retrans,
    );
    let mut ifaces = snapshot.interfaces.clone();
    ifaces.sort_by(|a, b| {
        (b.rx_kbs + b.tx_kbs)
            .partial_cmp(&(a.rx_kbs + a.tx_kbs))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for itf in ifaces.iter().take(2) {
        net_text.push_str(&format!(
            "  {}  ↓{:.0}  ↑{:.0} KB/s  E{} dr{} mc{}\n",
            itf.name,
            itf.rx_kbs,
            itf.tx_kbs,
            itf.rx_errs + itf.tx_errs,
            itf.rx_drops + itf.tx_drops,
            itf.multicast,
        ));
    }
    net_text.push_str(&format!(
        "  Clock {} offset {}us  [ptp0:{}]",
        snapshot.clock.state, snapshot.clock.offset_us, snapshot.clock.ptp_clock,
    ));
    f.render_widget(
        Paragraph::new(net_text).block(
            Block::default()
                .title("Network / Clock Sync")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        ),
        right_sub_chunks[1],
    );

    // Process table
    let process_mode = state.process_mode;
    let selected = state.selected_process;
    let proc_sort = state.proc_sort;
    let proc_desc = state.proc_desc;
    let total = snapshot.processes.len();

    // Header labels with a sort arrow on the active column.
    let arrow = |col: ProcSort| -> &'static str {
        if proc_sort == col {
            if proc_desc { " v" } else { " ^" }
        } else {
            ""
        }
    };
    let header_labels = [
        format!("PID{}", arrow(ProcSort::Pid)),
        format!("USER{}", arrow(ProcSort::User)),
        format!("COMMAND{}", arrow(ProcSort::Command)),
        format!("CPU%{}", arrow(ProcSort::Cpu)),
        format!("RES MEM{}", arrow(ProcSort::Mem)),
        "AFF".to_string(),
    ];
    let header = Row::new(
        header_labels
            .iter()
            .map(|h| Cell::from(h.clone()).style(Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD))),
    )
    .style(Style::default().bg(Color::DarkGray))
    .height(1);

    let rows: Vec<Row> = snapshot
        .processes
        .iter()
        .map(|proc| {
            Row::new(vec![
                Cell::from(proc.pid.to_string()),
                Cell::from(proc.user.clone()),
                Cell::from(proc.name.clone()),
                Cell::from(format!("{:.1}%", proc.cpu_usage)),
                Cell::from(format!("{:.2} MB", proc.mem_res_kb as f64 / 1024.0)),
                Cell::from(proc.affinity.clone()),
            ])
            .height(1)
        })
        .collect();

    let table_title: String = if let Some(ref msg) = state.kill_feedback {
        format!(" {} ", msg)
    } else if let Some(ref msg) = state.pin_feedback {
        format!(" {} ", msg)
    } else if process_mode {
        format!("Processes ({})  [↑↓ scroll  1-5 sort  0 reset  k kill  p pin  cc exit]", total)
    } else {
        format!("Processes ({})  [cc select/scroll  1-5 sort  0 reset]", total)
    };

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(7),
            Constraint::Percentage(10),
            Constraint::Percentage(38),
            Constraint::Percentage(9),
            Constraint::Percentage(18),
            Constraint::Percentage(18),
        ],
    )
    .header(header)
    .row_highlight_style(
        Style::default().bg(Color::Rgb(60, 60, 120)).fg(Color::White).add_modifier(Modifier::BOLD),
    )
    .block(
        Block::default()
            .title(table_title)
            .borders(Borders::ALL)
            .border_style(if process_mode {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default()
            }),
    );

    // Selection drives the viewport scroll; only highlight in select mode.
    state.proc_table_state.select(if process_mode { Some(selected) } else { None });
    f.render_stateful_widget(table, chunks[1], &mut state.proc_table_state);
    render_scrollbar(f, chunks[1], total, selected.min(total.saturating_sub(1)));
}

fn render_docker_tab(f: &mut ratatui::Frame, state: &mut AppState, area: Rect) {
    let snap = match state.docker_snapshot {
        Some(ref s) => s,
        None => {
            f.render_widget(
                Paragraph::new("\n  Connecting to Docker…").block(
                    Block::default().title("Docker").borders(Borders::ALL),
                ),
                area,
            );
            return;
        }
    };

    if !snap.available {
        let msg = format!(
            "\n  Docker is not available.\n\n  {}\n\n  Make sure the docker daemon is running and your user can\n  access /var/run/docker.sock (e.g. in the 'docker' group).",
            snap.error.as_deref().unwrap_or("could not connect to the docker socket"),
        );
        f.render_widget(
            Paragraph::new(msg).block(
                Block::default()
                    .title("Docker — unavailable")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Red)),
            ),
            area,
        );
        return;
    }

    // Logs overlay takes the whole content area.
    if state.show_logs {
        let name = state.selected_docker().map(|c| c.name.clone()).unwrap_or_default();
        let text = state.docker_logs.join("\n");
        f.render_widget(
            Paragraph::new(text)
                .scroll((state.log_scroll, 0))
                .block(
                    Block::default()
                        .title(format!(" Logs: {}  [↑↓ scroll   Esc/l close] ", name))
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Cyan)),
                ),
            area,
        );
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(40), Constraint::Min(0)].as_ref())
        .split(area);

    // Top: CPU chart | MEM chart | detail
    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(35), Constraint::Percentage(30)].as_ref())
        .split(chunks[0]);

    let selected = state.selected_docker();
    let empty: VecDeque<f64> = VecDeque::new();

    let (cpu_hist, mem_hist, cpu_title, mem_title, detail) = if let Some(c) = selected {
        let cpu_h = state.docker_cpu_hist.get(&c.id).unwrap_or(&empty);
        let mem_h = state.docker_mem_hist.get(&c.id).unwrap_or(&empty);
        let mem_pct = if c.mem_limit_mb > 0.0 { c.mem_mb / c.mem_limit_mb * 100.0 } else { 0.0 };
        let detail = format!(
            "\n  Name:   {}\n  Image:  {}\n  State:  {}\n  Status: {}\n  ID:     {}",
            c.name,
            c.image,
            c.state,
            c.status,
            c.id.chars().take(12).collect::<String>(),
        );
        (
            cpu_h.clone(),
            mem_h.clone(),
            format!("CPU  {:.1}%", c.cpu_usage),
            format!("MEM  {:.0}MB / {:.0}MB ({:.1}%)", c.mem_mb, c.mem_limit_mb, mem_pct),
            detail,
        )
    } else {
        (empty.clone(), empty.clone(), "CPU".to_string(), "MEM".to_string(), "\n  No container selected".to_string())
    };

    render_gradient_chart(f, &cpu_hist, cpu_title, top[0]);
    render_gradient_chart(f, &mem_hist, mem_title, top[1]);
    f.render_widget(
        Paragraph::new(detail).block(
            Block::default()
                .title("Container Detail")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Blue)),
        ),
        top[2],
    );

    // Container table
    let sel = state.selected_container;
    let dock_sort = state.dock_sort;
    let dock_desc = state.dock_desc;
    let total = snap.containers.len();

    let arrow = |col: DockSort| -> &'static str {
        if dock_sort == col {
            if dock_desc { " v" } else { " ^" }
        } else {
            ""
        }
    };
    let header_labels = [
        format!("NAME{}", arrow(DockSort::Name)),
        format!("IMAGE{}", arrow(DockSort::Image)),
        format!("STATE{}", arrow(DockSort::State)),
        format!("CPU%{}", arrow(DockSort::Cpu)),
        format!("MEM{}", arrow(DockSort::Mem)),
    ];
    let header = Row::new(
        header_labels
            .iter()
            .map(|h| Cell::from(h.clone()).style(Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD))),
    )
    .style(Style::default().bg(Color::DarkGray))
    .height(1);

    let rows: Vec<Row> = snap
        .containers
        .iter()
        .map(|c| {
            let state_color = match c.state.as_str() {
                "running" => Color::Green,
                "paused" => Color::Yellow,
                "exited" | "dead" => Color::Red,
                _ => Color::Gray,
            };
            Row::new(vec![
                Cell::from(c.name.clone()),
                Cell::from(c.image.clone()),
                Cell::from(c.state.clone()).style(Style::default().fg(state_color)),
                Cell::from(format!("{:.1}%", c.cpu_usage)),
                Cell::from(format!("{:.0}/{:.0} MB", c.mem_mb, c.mem_limit_mb)),
            ])
            .height(1)
        })
        .collect();

    let title: String = if let Some(ref msg) = state.docker_feedback {
        format!(" {} ", msg)
    } else {
        format!(
            "Containers ({})  [↑↓ nav  1-5 sort  0 reset  s start/stop  r restart  p pause  k kill  d remove  l logs]",
            total
        )
    };

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(24),
            Constraint::Percentage(34),
            Constraint::Percentage(12),
            Constraint::Percentage(12),
            Constraint::Percentage(18),
        ],
    )
    .header(header)
    .row_highlight_style(
        Style::default().bg(Color::Rgb(60, 60, 120)).fg(Color::White).add_modifier(Modifier::BOLD),
    )
    .block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );

    let select = if total > 0 { Some(sel.min(total - 1)) } else { None };
    state.dock_table_state.select(select);
    f.render_stateful_widget(table, chunks[1], &mut state.dock_table_state);
    render_scrollbar(f, chunks[1], total, sel.min(total.saturating_sub(1)));
}

fn render_action_bar(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    if state.show_sudo_prompt {
        let masked = "*".repeat(state.sudo_password.len());
        let (pid, name) = state
            .kill_target
            .as_ref()
            .map(|(p, n)| (*p, n.as_str()))
            .unwrap_or((0, "process"));
        let text = format!("  Type your password to kill {} (PID {}): {}_", name, pid, masked);
        f.render_widget(
            Paragraph::new(text).block(
                Block::default()
                    .title(" Sudo Required — [Enter] confirm   [Esc] cancel ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow)),
            ),
            area,
        );
    } else if state.show_kill_confirm {
        if let Some((pid, ref name)) = state.kill_target {
            let text = format!("  Kill {} (PID {})?    [y] Yes    [n] No    [Esc] Cancel", name, pid);
            f.render_widget(
                Paragraph::new(text).block(
                    Block::default()
                        .title(" Kill Process ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Red)),
                ),
                area,
            );
        }
    } else if let Some((action, _, ref name)) = state.docker_confirm {
        let text = format!("  {} container {}?    [y] Yes    [n] No    [Esc] Cancel", action.verb(), name);
        f.render_widget(
            Paragraph::new(text).block(
                Block::default()
                    .title(" Confirm Docker Action ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Red)),
            ),
            area,
        );
    }
}

/// Restores the terminal (raw mode, alternate screen, cursor) when dropped,
/// so cleanup runs on normal returns and unwinding panics. It will NOT run if
/// the binary is built with `panic = "abort"` or the process is killed.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    // From here on, the guard guarantees the terminal is restored on return/panic.
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Docker connection: one handle for actions in the main loop, a clone for the bg task.
    let docker_main: Option<Docker> = docker::connect();
    let docker_bg = docker_main.clone();

    let (tx, mut rx) = mpsc::channel::<SystemSnapshot>(1);
    let (dtx, mut drx) = mpsc::channel::<DockerSnapshot>(1);

    tokio::spawn(async move {
        // Don't panic if /proc is unavailable (non-Linux / sandbox): start from
        // zeroed baselines and let later reads recover once data is available.
        let mut prev_cpu = CpuRawState::read_from_system().unwrap_or(CpuRawState { total: 0, idle: 0, cores: Vec::new() });
        let mut prev_net = NetworkMetrics::read_from_system().unwrap_or(NetworkMetrics { rx_bytes: 0, tx_bytes: 0 });
        let mut prev_disk_io = DiskIOState::read_from_system().unwrap_or(DiskIOState { sectors_read: 0, sectors_written: 0 });
        let mut prev_proc_ticks: HashMap<u32, u64> = HashMap::new();
        let mut prev_docker_cpu: HashMap<String, (u64, u64)> = HashMap::new();
        let mut prev_interfaces: Vec<InterfaceMetrics> = Vec::new();
        let mut last_tick = Instant::now();

        loop {
            sleep(Duration::from_secs(1)).await;

            let now = Instant::now();
            let duration_secs = now.duration_since(last_tick).as_secs_f64();
            last_tick = now;

            let current_cpu = CpuRawState::read_from_system().unwrap_or_else(|_| prev_cpu.clone());
            let cpu_ptc = CpuRawState::calculate_usage(&prev_cpu, &current_cpu);
            let core_usage = CpuRawState::calculate_usage_per_core(&prev_cpu, &current_cpu);
            let total_ticks_delta = current_cpu.total.saturating_sub(prev_cpu.total);
            prev_cpu = current_cpu;

            let mem = MemoryMetrics::read_from_system().unwrap_or(MemoryMetrics {
                total_mem_kb: 0,
                used_mem_kb: 0,
                total_swap_kb: 0,
                used_swap_kb: 0,
            });

            let current_net = NetworkMetrics::read_from_system().unwrap_or(prev_net);
            let (rx_speed, tx_speed) = NetworkMetrics::calculate_speed(&prev_net, &current_net, duration_secs);
            prev_net = current_net;

            let current_disk_io = DiskIOState::read_from_system().unwrap_or(prev_disk_io);
            let disk_io = DiskIOState::calculate_speed(&prev_disk_io, &current_disk_io, duration_secs);
            prev_disk_io = current_disk_io;

            let processes = ProcessMetrics::fetch_all(&mut prev_proc_ticks, total_ticks_delta).unwrap_or_else(|_| Vec::new());

            let core_info = CoreInfo::read_from_system().unwrap_or_else(|_| CoreInfo {
                cpu_model: String::from("Unknown"),
                vendor: String::from("Unknown"),
                physical_core: 0,
            });

            let load_avg = LoadAverage::read_from_system().unwrap_or(LoadAverage { one: 0.0, five: 0.0, fifteen: 0.0 });
            let disk_partitions = DiskPartition::fetch_all();
            let numa_nodes = fetch_numa_nodes();
            let hugepages = HugePageInfo::fetch_all();
            let current_interfaces = InterfaceMetrics::read_all();
            let interfaces = compute_interface_speeds(&prev_interfaces, &current_interfaces, duration_secs);
            prev_interfaces = current_interfaces;
            let tcp_retrans = tcp_retransmits();
            let clock = read_clock_sync();

            let snapshot = SystemSnapshot {
                cpu_usage: cpu_ptc,
                core_usage,
                numa_nodes,
                hugepages,
                interfaces,
                tcp_retrans,
                clock,
                mem,
                net_rx_speed: rx_speed,
                net_tx_speed: tx_speed,
                processes,
                core_info,
                load_avg,
                disk_io,
                disk_partitions,
            };

            let _ = tx.try_send(snapshot);

            // Docker snapshot (if a daemon connection exists)
            let dsnap = if let Some(ref d) = docker_bg {
                docker::collect(d, &mut prev_docker_cpu).await
            } else {
                DockerSnapshot {
                    available: false,
                    error: Some("docker socket not reachable".to_string()),
                    containers: Vec::new(),
                }
            };
            let _ = dtx.try_send(dsnap);
        }
    });

    let mut state = AppState::new();
    let mut event_stream = EventStream::new();

    loop {
        while let Ok(snapshot) = rx.try_recv() {
            state.push_snapshot(snapshot);
        }
        while let Ok(dsnap) = drx.try_recv() {
            state.push_docker_snapshot(dsnap);
        }

        let bar_height = if state.show_kill_confirm
            || state.show_sudo_prompt
            || state.docker_confirm.is_some()
        {
            3u16
        } else {
            0
        };

        terminal.draw(|f| {
            let outer = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Min(0),
                    Constraint::Length(bar_height),
                ])
                .split(f.area());

            let tab = state.active_tab;
            render_tab_bar(f, outer[0], tab);

            match tab {
                Tab::System => render_system_tab(f, &mut state, outer[1]),
                Tab::Docker => render_docker_tab(f, &mut state, outer[1]),
            }

            render_action_bar(f, &state, outer[2]);
        })?;

        tokio::select! {
            maybe_event = event_stream.next() => {
                if let Some(Ok(Event::Key(key))) = maybe_event {
                    if !state.show_sudo_prompt {
                        state.kill_feedback = None;
                    }
                    state.docker_feedback = None;

                    match key.code {
                        // ── Esc: universal close ──────────────────────────────
                        KeyCode::Esc => {
                            if state.show_sudo_prompt {
                                state.show_sudo_prompt = false;
                                state.sudo_password.clear();
                                state.kill_target = None;
                            } else if state.show_kill_confirm {
                                state.show_kill_confirm = false;
                                state.kill_target = None;
                            } else if state.docker_confirm.is_some() {
                                state.docker_confirm = None;
                            } else if state.show_logs {
                                state.show_logs = false;
                                state.log_scroll = 0;
                            } else if state.process_mode {
                                state.process_mode = false;
                                state.pending_c = false;
                            }
                        }

                        // ── Sudo password bar (process kill) ──────────────────
                        KeyCode::Backspace if state.show_sudo_prompt => { state.sudo_password.pop(); }
                        KeyCode::Enter if state.show_sudo_prompt => {
                            if let Some((pid, ref name)) = state.kill_target.clone() {
                                let mut child = Command::new("sudo")
                                    .args(["-S", "kill", "-9", &pid.to_string()])
                                    .stdin(Stdio::piped())
                                    .stdout(Stdio::null())
                                    .stderr(Stdio::null())
                                    .spawn();
                                match child {
                                    Ok(ref mut c) => {
                                        if let Some(mut stdin) = c.stdin.take() {
                                            let _ = stdin.write_all(format!("{}\n", state.sudo_password).as_bytes());
                                        }
                                        let ok = c.wait().map(|s| s.success()).unwrap_or(false);
                                        state.sudo_password.clear();
                                        if ok {
                                            state.kill_feedback = Some(format!("Killed {} (PID {}) via sudo", name, pid));
                                            state.kill_target = None;
                                            state.show_sudo_prompt = false;
                                        } else {
                                            state.kill_feedback = Some("Wrong password, try again".to_string());
                                        }
                                    }
                                    Err(e) => {
                                        state.kill_feedback = Some(format!("sudo error: {}", e));
                                        state.kill_target = None;
                                        state.show_sudo_prompt = false;
                                        state.sudo_password.clear();
                                    }
                                }
                            }
                        }
                        KeyCode::Char(c) if state.show_sudo_prompt => { state.sudo_password.push(c); }
                        _ if state.show_sudo_prompt => {}

                        // ── Process kill confirm ──────────────────────────────
                        KeyCode::Char('y') if state.show_kill_confirm => {
                            state.show_kill_confirm = false;
                            if let Some((pid, ref name)) = state.kill_target.clone() {
                                let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
                                if result == 0 {
                                    state.kill_feedback = Some(format!("Killed {} (PID {})", name, pid));
                                    state.kill_target = None;
                                } else {
                                    let err = std::io::Error::last_os_error();
                                    if err.raw_os_error() == Some(libc::EPERM) {
                                        state.show_sudo_prompt = true;
                                    } else {
                                        state.kill_feedback = Some(format!("Kill failed: {}", err));
                                        state.kill_target = None;
                                    }
                                }
                            }
                        }
                        KeyCode::Char('n') if state.show_kill_confirm => {
                            state.show_kill_confirm = false;
                            state.kill_target = None;
                        }
                        _ if state.show_kill_confirm => {}

                        // ── Docker action confirm (kill/remove) ───────────────
                        KeyCode::Char('y') if state.docker_confirm.is_some() => {
                            if let Some((action, id, name)) = state.docker_confirm.take() {
                                if let Some(ref d) = docker_main {
                                    match docker::run_action(d, action, &id, &name).await {
                                        Ok(msg) => state.docker_feedback = Some(msg),
                                        Err(e) => state.docker_feedback = Some(e),
                                    }
                                }
                            }
                        }
                        KeyCode::Char('n') if state.docker_confirm.is_some() => {
                            state.docker_confirm = None;
                        }
                        _ if state.docker_confirm.is_some() => {}

                        // ── Logs scrolling ────────────────────────────────────
                        KeyCode::Up if state.show_logs => {
                            state.log_scroll = state.log_scroll.saturating_sub(1);
                        }
                        KeyCode::Down if state.show_logs => {
                            state.log_scroll = state.log_scroll.saturating_add(1);
                        }
                        KeyCode::Char('l') if state.show_logs => {
                            state.show_logs = false;
                            state.log_scroll = 0;
                        }
                        _ if state.show_logs => {}

                        // ── Global ────────────────────────────────────────────
                        KeyCode::Char('q') => break,

                        KeyCode::Char('m') => {
                            if state.pending_m {
                                state.active_tab = match state.active_tab {
                                    Tab::System => Tab::Docker,
                                    Tab::Docker => Tab::System,
                                };
                                state.pending_m = false;
                            } else {
                                state.pending_m = true;
                            }
                        }

                        // ── Reset sort to default (0) ─────────────────────────
                        KeyCode::Char('0') if state.active_tab == Tab::System => {
                            state.pending_c = false;
                            state.pending_m = false;
                            state.proc_sort = ProcSort::Mem;
                            state.proc_desc = true;
                            state.sort_processes();
                            state.selected_process = 0;
                        }
                        KeyCode::Char('0') if state.active_tab == Tab::Docker => {
                            state.pending_m = false;
                            state.dock_sort = DockSort::Name;
                            state.dock_desc = false;
                            state.sort_containers();
                            state.selected_container = 0;
                        }

                        // ── Sort columns (1-5) per tab ────────────────────────
                        KeyCode::Char(c @ '1'..='5') if state.active_tab == Tab::System => {
                            state.pending_c = false;
                            state.pending_m = false;
                            let col = match c {
                                '1' => ProcSort::Pid,
                                '2' => ProcSort::User,
                                '3' => ProcSort::Command,
                                '4' => ProcSort::Cpu,
                                _ => ProcSort::Mem,
                            };
                            if state.proc_sort == col {
                                state.proc_desc = !state.proc_desc;
                            } else {
                                state.proc_sort = col;
                                // numeric columns default to descending, text to ascending
                                state.proc_desc = matches!(col, ProcSort::Cpu | ProcSort::Mem | ProcSort::Pid);
                            }
                            state.sort_processes();
                            state.selected_process = 0;
                        }
                        KeyCode::Char(c @ '1'..='5') if state.active_tab == Tab::Docker => {
                            state.pending_m = false;
                            let col = match c {
                                '1' => DockSort::Name,
                                '2' => DockSort::Image,
                                '3' => DockSort::State,
                                '4' => DockSort::Cpu,
                                _ => DockSort::Mem,
                            };
                            if state.dock_sort == col {
                                state.dock_desc = !state.dock_desc;
                            } else {
                                state.dock_sort = col;
                                state.dock_desc = matches!(col, DockSort::Cpu | DockSort::Mem);
                            }
                            state.sort_containers();
                            state.selected_container = 0;
                        }

                        // ── Docker tab keys ───────────────────────────────────
                        KeyCode::Up if state.active_tab == Tab::Docker => {
                            state.pending_m = false;
                            let count = state.container_count();
                            if count > 0 {
                                state.selected_container = if state.selected_container == 0 {
                                    count - 1
                                } else {
                                    state.selected_container - 1
                                };
                            }
                        }
                        KeyCode::Down if state.active_tab == Tab::Docker => {
                            state.pending_m = false;
                            let count = state.container_count();
                            if count > 0 {
                                state.selected_container = (state.selected_container + 1) % count;
                            }
                        }
                        KeyCode::Char('s') if state.active_tab == Tab::Docker => {
                            state.pending_m = false;
                            let target = state.selected_docker().map(|c| (c.state.clone(), c.id.clone(), c.name.clone()));
                            if let Some((cstate, id, name)) = target {
                                let action = if cstate == "running" { DockerAction::Stop } else { DockerAction::Start };
                                if let Some(ref d) = docker_main {
                                    state.docker_feedback = Some(match docker::run_action(d, action, &id, &name).await {
                                        Ok(msg) => msg,
                                        Err(e) => e,
                                    });
                                }
                            }
                        }
                        KeyCode::Char('r') if state.active_tab == Tab::Docker => {
                            state.pending_m = false;
                            let target = state.selected_docker().map(|c| (c.id.clone(), c.name.clone()));
                            if let Some((id, name)) = target {
                                if let Some(ref d) = docker_main {
                                    state.docker_feedback = Some(match docker::run_action(d, DockerAction::Restart, &id, &name).await {
                                        Ok(msg) => msg,
                                        Err(e) => e,
                                    });
                                }
                            }
                        }
                        KeyCode::Char('p') if state.active_tab == Tab::Docker => {
                            state.pending_m = false;
                            let target = state.selected_docker().map(|c| (c.state.clone(), c.id.clone(), c.name.clone()));
                            if let Some((cstate, id, name)) = target {
                                let action = if cstate == "paused" { DockerAction::Unpause } else { DockerAction::Pause };
                                if let Some(ref d) = docker_main {
                                    state.docker_feedback = Some(match docker::run_action(d, action, &id, &name).await {
                                        Ok(msg) => msg,
                                        Err(e) => e,
                                    });
                                }
                            }
                        }
                        KeyCode::Char('k') if state.active_tab == Tab::Docker => {
                            state.pending_m = false;
                            let target = state.selected_docker().map(|c| (c.id.clone(), c.name.clone()));
                            if let Some((id, name)) = target {
                                state.docker_confirm = Some((DockerAction::Kill, id, name));
                            }
                        }
                        KeyCode::Char('d') if state.active_tab == Tab::Docker => {
                            state.pending_m = false;
                            let target = state.selected_docker().map(|c| (c.id.clone(), c.name.clone()));
                            if let Some((id, name)) = target {
                                state.docker_confirm = Some((DockerAction::Remove, id, name));
                            }
                        }
                        KeyCode::Char('l') if state.active_tab == Tab::Docker => {
                            state.pending_m = false;
                            let id = state.selected_docker().map(|c| c.id.clone());
                            if let Some(id) = id {
                                if let Some(ref d) = docker_main {
                                    let logs = docker::fetch_logs(d, &id, DOCKER_LOG_TAIL).await;
                                    state.docker_logs = if logs.is_empty() {
                                        vec!["(no logs)".to_string()]
                                    } else {
                                        logs
                                    };
                                    state.log_scroll = 0;
                                    state.show_logs = true;
                                }
                            }
                        }

                        // ── System tab keys ───────────────────────────────────
                        KeyCode::Char('k') if state.active_tab == Tab::System && state.process_mode => {
                            state.pending_c = false;
                            if let Some(ref snap) = state.last_snapshot {
                                if let Some(proc) = snap.processes.get(state.selected_process) {
                                    if proc.pid == std::process::id() {
                                        state.kill_feedback = Some("Cannot kill panda itself".to_string());
                                    } else {
                                        state.kill_target = Some((proc.pid, proc.name.clone()));
                                        state.show_kill_confirm = true;
                                    }
                                }
                            }
                        }
                        KeyCode::Char('p') if state.active_tab == Tab::System && state.process_mode => {
                            state.pending_c = false;
                            let target = state.last_snapshot.as_ref().and_then(|snap| {
                                snap.processes.get(state.selected_process).map(|proc| {
                                    let (pid, name) = (proc.pid, proc.name.clone());
                                    let least = snap
                                        .core_usage
                                        .iter()
                                        .enumerate()
                                        .min_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
                                        .map(|(idx, _)| idx);
                                    (pid, name, least)
                                })
                            });
                            if let Some((pid, name, least)) = target {
                                match least {
                                    Some(core) => match ProcessMetrics::set_affinity(pid, core as u32) {
                                        Ok(_) => state.pin_feedback = Some(format!(
                                            "Pinned {} (PID {}) to core {}",
                                            name, pid, core,
                                        )),
                                        Err(e) => state.pin_feedback = Some(format!("Pin failed: {}", e)),
                                    },
                                    None => state.pin_feedback = Some("No CPU cores detected".to_string()),
                                }
                            }
                        }
                        KeyCode::Char('c') if state.active_tab == Tab::System => {
                            if state.pending_c {
                                state.process_mode = !state.process_mode;
                                if state.process_mode { state.selected_process = 0; }
                                state.pending_c = false;
                            } else {
                                state.pending_c = true;
                            }
                        }
                        KeyCode::Up if state.active_tab == Tab::System && state.process_mode => {
                            state.pending_c = false;
                            let count = state.visible_process_count();
                            if count > 0 {
                                state.selected_process = if state.selected_process == 0 {
                                    count - 1
                                } else {
                                    state.selected_process - 1
                                };
                            }
                        }
                        KeyCode::Down if state.active_tab == Tab::System && state.process_mode => {
                            state.pending_c = false;
                            let count = state.visible_process_count();
                            if count > 0 {
                                state.selected_process = (state.selected_process + 1) % count;
                            }
                        }

                        _ => {
                            state.pending_c = false;
                            state.pending_m = false;
                        }
                    }
                }
            }
            _ = sleep(Duration::from_millis(50)) => {}
        }
    }

    // Terminal restoration is handled by TerminalGuard's Drop.
    Ok(())
}
