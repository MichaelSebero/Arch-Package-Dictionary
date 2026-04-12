use std::collections::HashMap;
use std::env;
use std::io::{self, Write};
use std::process::Stdio;

use crossterm::{
    cursor,
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, MouseEventKind},
    execute,
    terminal::{self, DisableLineWrap, EnableLineWrap, EnterAlternateScreen, LeaveAlternateScreen},
};
use tokio::process::Command as TokioCommand;

const BOLD: &str = "\x1B[1m";
const BLUE: &str = "\x1B[34m";
const RED: &str = "\x1B[31m";
const GREEN: &str = "\x1B[32m";
const RESET: &str = "\x1B[0m";
const DIM: &str = "\x1B[2m";

#[derive(Clone, Debug)]
struct Package {
    name: String,
    version: String,
    description: String,
}

impl Package {
    fn new(name: impl Into<String>, version: impl Into<String>, description: impl Into<String>) -> Self {
        Self { name: name.into(), version: version.into(), description: description.into() }
    }

    fn has_description(&self) -> bool {
        !self.description.is_empty() && self.description != "No description."
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("{}Usage:{} pd <search-term>", BOLD, RESET);
        std::process::exit(1);
    }

    let search_term = args[1..].join(" ");

    match search_all_sources(&search_term).await {
        Ok((pacman, aur, flatpak)) => print_results(&pacman, &aur, &flatpak),
        Err(e) => {
            eprintln!("{}Error:{} {}", RED, RESET, e);
            std::process::exit(1);
        }
    }
}

// ─── Search ───────────────────────────────────────────────────────────────────

async fn search_all_sources(
    term: &str,
) -> Result<(Vec<Package>, Vec<Package>, Vec<Package>), Box<dyn std::error::Error>> {
    let timeout = std::time::Duration::from_secs(10);
    let (pacman_result, aur_result, flatpak_result) = tokio::join!(
        tokio::time::timeout(timeout, search_pacman(term)),
        tokio::time::timeout(timeout, search_aur(term)),
        tokio::time::timeout(timeout, search_flatpak(term))
    );
    Ok((
        handle_search_result(pacman_result, "Pacman"),
        handle_search_result(aur_result, "AUR"),
        handle_search_result(flatpak_result, "Flatpak"),
    ))
}

fn handle_search_result(
    result: Result<Result<Vec<Package>, std::io::Error>, tokio::time::error::Elapsed>,
    source: &str,
) -> Vec<Package> {
    match result {
        Ok(Ok(packages)) => dedupe_packages(packages),
        Ok(Err(e)) => { eprintln!("{}Warning:{} {} search failed: {}", RED, RESET, source, e); Vec::new() }
        Err(_)     => { eprintln!("{}Warning:{} {} search timed out",  RED, RESET, source);    Vec::new() }
    }
}

fn dedupe_packages(packages: Vec<Package>) -> Vec<Package> {
    let mut best: HashMap<String, Package> = HashMap::with_capacity(packages.len());
    for pkg in packages {
        best.entry(pkg.name.clone())
            .and_modify(|existing| { if is_better_package(&pkg, existing) { *existing = pkg.clone(); } })
            .or_insert(pkg);
    }
    let mut result: Vec<Package> = best.into_values().collect();
    result.sort_by(|a, b| a.name.cmp(&b.name));
    result
}

fn is_better_package(new: &Package, existing: &Package) -> bool {
    match (new.has_description(), existing.has_description()) {
        (true, false) => true,
        (false, true) => false,
        _ => new.version > existing.version,
    }
}

async fn search_pacman(term: &str) -> std::io::Result<Vec<Package>> {
    let output = TokioCommand::new("pacman").args(["-Ss", term]).output().await?;
    Ok(parse_pair_output(&String::from_utf8_lossy(&output.stdout), None))
}

async fn search_aur(term: &str) -> std::io::Result<Vec<Package>> {
    let helper = if command_exists("paru").await { "paru" }
                 else if command_exists("yay").await { "yay" }
                 else { return Ok(Vec::new()); };
    let output = TokioCommand::new(helper).args(["-Ss", "--aur", term]).output().await?;
    Ok(parse_pair_output(&String::from_utf8_lossy(&output.stdout), Some("aur/")))
}

fn parse_pair_output(stdout: &str, prefix_filter: Option<&str>) -> Vec<Package> {
    let mut results = Vec::new();
    let mut lines = stdout.lines();
    while let Some(line) = lines.next() {
        if !line.contains('/') { continue; }
        if let Some(prefix) = prefix_filter { if !line.contains(prefix) { continue; } }
        if let Some(pkg) = parse_package_header(line, &mut lines) { results.push(pkg); }
    }
    results
}

fn parse_package_header(line: &str, lines: &mut std::str::Lines) -> Option<Package> {
    let after_slash = line.splitn(2, '/').nth(1)?;
    let mut parts   = after_slash.splitn(2, ' ');
    let name        = parts.next()?.trim().to_string();
    let rest        = parts.next()?.trim();
    let version     = rest.split_whitespace().next().unwrap_or("unknown").to_string();
    let description = lines.next()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| "No description.".to_string());
    Some(Package::new(name, version, description))
}

async fn search_flatpak(term: &str) -> std::io::Result<Vec<Package>> {
    if !command_exists("flatpak").await { return Ok(Vec::new()); }
    let output = TokioCommand::new("flatpak")
        .args(["search", "--columns=name,application,version,description", term])
        .output().await?;
    Ok(parse_flatpak_output(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_flatpak_output(stdout: &str) -> Vec<Package> {
    stdout.lines().skip(1)
        .filter(|l| !l.trim().is_empty() && !l.starts_with("No matches"))
        .filter_map(|line| {
            let p: Vec<&str> = line.split('\t').collect();
            if p.len() < 4 { return None; }
            let (name, app_id, version, desc) = (p[0].trim(), p[1].trim(), p[2].trim(), p[3].trim());
            if name.is_empty() && app_id.is_empty() { return None; }
            Some(Package::new(
                if app_id.is_empty() { name.to_string() } else { format!("{} ({})", name, app_id) },
                if version.is_empty() { "unknown".to_string() } else { version.to_string() },
                if desc.is_empty() { "No description.".to_string() } else { desc.to_string() },
            ))
        })
        .collect()
}

async fn command_exists(cmd: &str) -> bool {
    TokioCommand::new("which").arg(cmd)
        .stdout(Stdio::null()).stderr(Stdio::null())
        .output().await.map(|o| o.status.success()).unwrap_or(false)
}

// ─── Output ───────────────────────────────────────────────────────────────────

fn print_results(pacman: &[Package], aur: &[Package], flatpak: &[Package]) {
    if pacman.is_empty() && aur.is_empty() && flatpak.is_empty() {
        println!("No packages found.");
        return;
    }

    let mut output = format!(
        "{}Pacman:{} {} | {}AUR:{} {} | {}Flatpak:{} {}\n\n",
        BOLD, RESET, format_count(pacman.len()),
        BOLD, RESET, format_count(aur.len()),
        BOLD, RESET, format_count(flatpak.len()),
    );
    add_section(&mut output, "Pacman",  pacman,  BLUE);
    add_section(&mut output, "AUR",     aur,     RED);
    add_section(&mut output, "Flatpak", flatpak, GREEN);

    let lines: Vec<&str> = output.lines().collect();
    if let Err(e) = run_viewer(&lines) {
        eprintln!("{}Warning:{} viewer error: {}", RED, RESET, e);
        print!("{}", output); // fallback: plain stdout
    }
}

fn format_count(count: usize) -> String {
    if count == 1 { "1 package".to_string() } else { format!("{} packages", count) }
}

fn add_section(output: &mut String, name: &str, packages: &[Package], color: &str) {
    if packages.is_empty() { return; }
    output.push_str(&format!(
        "{}{}{} Results:{}\n{}\n",
        BOLD, color, name, RESET, "=".repeat(name.len() + 9)
    ));
    for pkg in packages {
        output.push_str(&format!(
            "  {}{}{}{}\n  {}\n  {}Version:{} {}\n\n",
            BOLD, color, pkg.name, RESET,
            pkg.description,
            BOLD, RESET, pkg.version,
        ));
    }
}

// ─── TUI Viewer ───────────────────────────────────────────────────────────────

fn run_viewer(lines: &[&str]) -> std::io::Result<()> {
    let mut stdout = io::stdout();
    terminal::enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture, cursor::Hide, DisableLineWrap)?;

    let result = viewer_loop(&mut stdout, lines);

    // Always restore terminal state even if the loop errored
    let _ = execute!(stdout, EnableLineWrap, LeaveAlternateScreen, DisableMouseCapture, cursor::Show);
    let _ = terminal::disable_raw_mode();
    result
}

fn viewer_loop(stdout: &mut impl Write, lines: &[&str]) -> std::io::Result<()> {
    let mut offset: usize = 0;
    let mut dragging = false;

    loop {
        let (cols, rows) = terminal::size()?;
        let view_h     = rows.saturating_sub(1) as usize;
        let scroll_col = cols.saturating_sub(1);
        let total      = lines.len();
        let max_offset = total.saturating_sub(view_h);
        offset         = offset.min(max_offset);

        // thumb_h and track_h are computed once per frame and reused for both
        // rendering and the drag→offset inverse calculation, keeping them in sync.
        let thumb_h = if total <= view_h {
            view_h
        } else {
            ((view_h * view_h) / total).max(1).min(view_h)
        };
        let track_h   = view_h.saturating_sub(thumb_h); // rows the thumb can travel
        let thumb_top = if track_h == 0 { 0 } else { offset * track_h / max_offset };

        // ── Content ──────────────────────────────────────────────────────────
        for row in 0..view_h {
            execute!(stdout, cursor::MoveTo(0, row as u16))?;
            write!(stdout, "\x1B[0m\x1B[2K")?;
            if let Some(line) = lines.get(offset + row) {
                write!(stdout, "{}", line)?;
            }
        }

        // ── Scrollbar ────────────────────────────────────────────────────────
        for row in 0..view_h {
            execute!(stdout, cursor::MoveTo(scroll_col, row as u16))?;
            write!(stdout, "\x1B[0m")?;
            if row >= thumb_top && row < thumb_top + thumb_h {
                write!(stdout, "█")?;
            } else {
                write!(stdout, "{}│{}", DIM, RESET)?;
            }
        }

        // ── Status bar ───────────────────────────────────────────────────────
        execute!(stdout, cursor::MoveTo(0, rows - 1))?;
        write!(stdout,
            "\x1B[0m\x1B[7m  {}/{} lines  │  ↑↓ / PgUp PgDn / scroll  │  q to quit  \x1B[0m",
            (offset + 1).min(total), total
        )?;

        stdout.flush()?;

        // ── Events ───────────────────────────────────────────────────────────
        // Drain all queued events before re-rendering so fast scrolling doesn't
        // fall behind. We only break to re-render when the queue is empty or
        // after a drag event (to stay responsive).
        loop {
            match event::read()? {
                Event::Key(k) => match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Up   | KeyCode::Char('k') => { offset = offset.saturating_sub(1); }
                    KeyCode::Down | KeyCode::Char('j') => { offset = (offset + 1).min(max_offset); }
                    KeyCode::PageUp   | KeyCode::Char('b') => { offset = offset.saturating_sub(view_h); }
                    KeyCode::PageDown | KeyCode::Char('f') => { offset = (offset + view_h).min(max_offset); }
                    KeyCode::Home | KeyCode::Char('g') => { offset = 0; }
                    KeyCode::End  | KeyCode::Char('G') => { offset = max_offset; }
                    _ => { dragging = false; continue; }
                },
                Event::Mouse(m) => match m.kind {
                    MouseEventKind::ScrollUp   => { offset = offset.saturating_sub(3); }
                    MouseEventKind::ScrollDown => { offset = (offset + 3).min(max_offset); }

                    // Button released — stop dragging
                    MouseEventKind::Up(_) => { dragging = false; continue; }

                    // Click on scrollbar starts a drag and jumps to position
                    MouseEventKind::Down(_) if m.column == scroll_col => {
                        dragging = true;
                        // Inverse of: thumb_top = offset * track_h / max_offset
                        // Clamp row into [0, track_h] so the thumb never overshoots
                        let row = (m.row as usize).min(view_h.saturating_sub(1));
                        offset = if track_h == 0 { 0 }
                                 else { (row * max_offset / track_h).min(max_offset) };
                    }

                    // Drag anywhere while dragging (mouse may leave the scroll column)
                    MouseEventKind::Drag(_) if dragging => {
                        let row = (m.row as usize).min(view_h.saturating_sub(1));
                        offset = if track_h == 0 { 0 }
                                 else { (row * max_offset / track_h).min(max_offset) };
                        // Re-render immediately on every drag tick for smoothness,
                        // but keep draining if more drag events are already queued.
                        if !event::poll(std::time::Duration::ZERO).unwrap_or(false) {
                            break;
                        }
                        continue;
                    }

                    _ => { dragging = false; continue; }
                },
                Event::Resize(_, _) => { dragging = false; }
                _ => continue,
            }
            break;
        }
    }
}
