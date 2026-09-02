use std::collections::HashMap;
use std::env;
use std::io::{self, Write};
use std::process::Stdio;
use std::time::Duration;

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
const YELLOW: &str = "\x1B[33m";
const MAGENTA: &str = "\x1B[35m";
const CYAN: &str = "\x1B[36m";
const RESET: &str = "\x1B[0m";
const DIM: &str = "\x1B[2m";

// Every source here just queries an already-built local cache or makes one
// quick network call, so a single shared timeout is enough for all of them.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(10);

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
    let results = search_all_sources(&search_term).await;

    if results.is_empty() {
        eprintln!("{}Error:{} no supported package manager found on this system.", RED, RESET);
        eprintln!("Supported: pacman, paru/yay, xbps-query, apt-cache, dnf/yum, flatpak.");
        std::process::exit(1);
    }

    print_results(&results);
}

// ─── Detection ──────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
enum Source {
    Pacman,
    Aur(&'static str),
    Xbps,
    Apt,
    YumDnf(&'static str),
    Flatpak,
}

impl Source {
    fn display_name(&self) -> String {
        match self {
            Source::Pacman => "Pacman".to_string(),
            Source::Aur(helper) => format!("AUR ({})", helper),
            Source::Xbps => "XBPS".to_string(),
            Source::Apt => "APT".to_string(),
            Source::YumDnf(bin) => bin.to_uppercase(),
            Source::Flatpak => "Flatpak".to_string(),
        }
    }

    fn color(&self) -> &'static str {
        match self {
            Source::Pacman => BLUE,
            Source::Aur(_) => RED,
            Source::Xbps => YELLOW,
            Source::Apt => MAGENTA,
            Source::YumDnf(_) => CYAN,
            Source::Flatpak => GREEN,
        }
    }
}

// Probes the system for every backend we know how to talk to. All checks run
// concurrently; the fixed ordering of the pushes below (not completion order)
// determines the display order later, so output stays stable across runs.
async fn detect_sources() -> Vec<Source> {
    let (pacman, aur, xbps, apt, yum_dnf, flatpak) = tokio::join!(
        command_exists("pacman"),
        detect_aur_helper(),
        command_exists("xbps-query"),
        command_exists("apt-cache"),
        detect_yum_dnf(),
        command_exists("flatpak"),
    );

    let mut sources = Vec::new();
    if pacman { sources.push(Source::Pacman); }
    if let Some(helper) = aur { sources.push(Source::Aur(helper)); }
    if xbps { sources.push(Source::Xbps); }
    if apt { sources.push(Source::Apt); }
    if let Some(bin) = yum_dnf { sources.push(Source::YumDnf(bin)); }
    if flatpak { sources.push(Source::Flatpak); }
    sources
}

async fn detect_aur_helper() -> Option<&'static str> {
    if command_exists("paru").await { Some("paru") }
    else if command_exists("yay").await { Some("yay") }
    else { None }
}

async fn detect_yum_dnf() -> Option<&'static str> {
    if command_exists("dnf").await { Some("dnf") }
    else if command_exists("yum").await { Some("yum") }
    else { None }
}

async fn command_exists(cmd: &str) -> bool {
    TokioCommand::new("which").arg(cmd)
        .stdout(Stdio::null()).stderr(Stdio::null())
        .output().await.map(|o| o.status.success()).unwrap_or(false)
}

// ─── Search ───────────────────────────────────────────────────────────────────

#[derive(Debug)]
enum SearchOutcome {
    Found(Vec<Package>),
    TimedOut,
    Failed(String),
}

async fn search_all_sources(term: &str) -> Vec<(Source, SearchOutcome)> {
    let sources = detect_sources().await;
    let term_owned = term.to_string();

    let handles: Vec<_> = sources.into_iter().map(|source| {
        let term = term_owned.clone();
        tokio::spawn(async move {
            let result = tokio::time::timeout(SEARCH_TIMEOUT, run_search(source, &term)).await;
            let outcome = handle_search_result(result, &source.display_name());
            (source, outcome)
        })
    }).collect();

    let mut results = Vec::with_capacity(handles.len());
    for handle in handles {
        match handle.await {
            Ok(pair) => results.push(pair),
            Err(e) => eprintln!("{}Warning:{} a search task panicked: {}", RED, RESET, e),
        }
    }
    results
}

async fn run_search(source: Source, term: &str) -> std::io::Result<Vec<Package>> {
    match source {
        Source::Pacman => search_pacman(term).await,
        Source::Aur(helper) => search_aur(helper, term).await,
        Source::Xbps => search_xbps(term).await,
        Source::Apt => search_apt(term).await,
        Source::YumDnf(bin) => search_yum_dnf(bin, term).await,
        Source::Flatpak => search_flatpak(term).await,
    }
}

fn handle_search_result(
    result: Result<Result<Vec<Package>, std::io::Error>, tokio::time::error::Elapsed>,
    source: &str,
) -> SearchOutcome {
    match result {
        Ok(Ok(packages)) => SearchOutcome::Found(dedupe_packages(packages)),
        Ok(Err(e)) => {
            eprintln!("{}Warning:{} {} search failed: {}", RED, RESET, source, e);
            SearchOutcome::Failed(e.to_string())
        }
        Err(_) => {
            eprintln!("{}Warning:{} {} search timed out", RED, RESET, source);
            SearchOutcome::TimedOut
        }
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

// Runs `program` and returns its stdout as UTF-8. Deliberately does NOT
// treat a non-zero exit as failure -- pacman exits non-zero for a perfectly
// normal "nothing matched", so doing that would misreport a chunk of clean,
// empty results as errors. What IS a reliable signal, since a genuine "no
// matches" is normally silent on both streams, is stdout coming back empty
// while stderr has something in it -- that combination means the tool
// actually had something to say.
async fn run_and_capture(program: &str, args: &[&str]) -> std::io::Result<String> {
    let output = TokioCommand::new(program).args(args).output().await?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();

    if stdout.trim().is_empty() && !stderr.is_empty() {
        let first_line = stderr.lines().next().unwrap_or(stderr);
        return Err(std::io::Error::new(std::io::ErrorKind::Other, first_line.to_string()));
    }
    Ok(stdout)
}

async fn search_pacman(term: &str) -> std::io::Result<Vec<Package>> {
    let stdout = run_and_capture("pacman", &["-Ss", term]).await?;
    Ok(parse_pair_output(&stdout, None))
}

async fn search_aur(helper: &str, term: &str) -> std::io::Result<Vec<Package>> {
    let stdout = run_and_capture(helper, &["-Ss", "--aur", term]).await?;
    Ok(parse_pair_output(&stdout, Some("aur/")))
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

async fn search_xbps(term: &str) -> std::io::Result<Vec<Package>> {
    let stdout = run_and_capture("xbps-query", &["-Rs", term]).await?;
    Ok(parse_xbps_output(&stdout))
}

// xbps-query -Rs prints one line per package: "[state] pkgver  description",
// e.g. "[-] zsh-5.9_6   A shell with lots of features".
fn parse_xbps_output(stdout: &str) -> Vec<Package> {
    stdout.lines().filter_map(|line| {
        let after_state = line.split_once(']')?.1.trim_start();
        let mut parts = after_state.splitn(2, char::is_whitespace);
        let pkgver = parts.next()?;
        if pkgver.is_empty() { return None; }
        let description = parts.next()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .unwrap_or("No description.")
            .to_string();
        let (name, version) = split_pkgver(pkgver);
        Some(Package::new(name, version, description))
    }).collect()
}

// XBPS "pkgver" strings are "name-version_revision", and the name itself may
// contain hyphens (e.g. "gtk-doc-1.33.2_1"), so we split at the last '-'
// that's immediately followed by a digit, since that's where the version
// starts.
fn split_pkgver(pkgver: &str) -> (String, String) {
    let bytes = pkgver.as_bytes();
    for i in (0..bytes.len().saturating_sub(1)).rev() {
        if bytes[i] == b'-' && bytes[i + 1].is_ascii_digit() {
            return (pkgver[..i].to_string(), pkgver[i + 1..].to_string());
        }
    }
    (pkgver.to_string(), "unknown".to_string())
}

async fn search_apt(term: &str) -> std::io::Result<Vec<Package>> {
    let stdout = run_and_capture("apt-cache", &["search", term]).await?;
    Ok(parse_apt_output(&stdout))
}

// apt-cache search prints "pkgname - description" (Debian package names can't
// contain spaces, so splitting on the first " - " is unambiguous). It doesn't
// give a version; getting one would mean an extra `apt-cache policy` call per
// result, which we skip for now to keep this to one process per source.
fn parse_apt_output(stdout: &str) -> Vec<Package> {
    stdout.lines().filter_map(|line| {
        let (name, description) = line.split_once(" - ")?;
        let name = name.trim();
        if name.is_empty() { return None; }
        let description = description.trim();
        let description = if description.is_empty() { "No description." } else { description };
        Some(Package::new(name, "unknown", description))
    }).collect()
}

async fn search_yum_dnf(binary: &str, term: &str) -> std::io::Result<Vec<Package>> {
    let stdout = run_and_capture(binary, &["search", term]).await?;
    Ok(parse_yum_dnf_output(&stdout))
}

const KNOWN_ARCHES: &[&str] = &["x86_64", "noarch", "i686", "aarch64", "armv7hl", "armv7l", "ppc64le", "s390x"];

// dnf/yum search prints "name.arch : description" lines mixed in with status
// and header text; filtering for lines containing " : " (space on both
// sides) skips those, since RPM names can't contain spaces either. No
// version here for the same reason as apt above.
fn parse_yum_dnf_output(stdout: &str) -> Vec<Package> {
    stdout.lines().filter_map(|line| {
        let (name_arch, description) = line.split_once(" : ")?;
        let name = strip_arch_suffix(name_arch.trim()).to_string();
        if name.is_empty() { return None; }
        let description = description.trim();
        let description = if description.is_empty() { "No description." } else { description };
        Some(Package::new(name, "unknown", description))
    }).collect()
}

fn strip_arch_suffix(name: &str) -> &str {
    for arch in KNOWN_ARCHES {
        if let Some(stripped) = name.strip_suffix(&format!(".{}", arch)) {
            return stripped;
        }
    }
    name
}

async fn search_flatpak(term: &str) -> std::io::Result<Vec<Package>> {
    let stdout = run_and_capture(
        "flatpak",
        &["search", "--columns=name,application,version,description", term],
    ).await?;
    Ok(parse_flatpak_output(&stdout))
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

// ─── Output ───────────────────────────────────────────────────────────────────

fn print_results(results: &[(Source, SearchOutcome)]) {
    let any_found = results.iter()
        .any(|(_, o)| matches!(o, SearchOutcome::Found(pkgs) if !pkgs.is_empty()));
    let any_trouble = results.iter()
        .any(|(_, o)| !matches!(o, SearchOutcome::Found(_)));

    // Only take the quick exit when every source cleanly ran and found
    // nothing. If anything timed out or failed, fall through so that shows
    // up in the summary and body instead of looking identical to "no
    // matches anywhere".
    if !any_found && !any_trouble {
        println!("No packages found.");
        return;
    }

    let summary = results.iter()
        .map(|(source, outcome)| {
            let status = match outcome {
                SearchOutcome::Found(pkgs) => format_count(pkgs.len()),
                SearchOutcome::TimedOut => format!("timed out after {}s", SEARCH_TIMEOUT.as_secs()),
                SearchOutcome::Failed(_) => "failed".to_string(),
            };
            format!("{}{}:{} {}", BOLD, source.display_name(), RESET, status)
        })
        .collect::<Vec<_>>()
        .join(" | ");
    let mut output = format!("{}\n\n", summary);

    for (source, outcome) in results {
        match outcome {
            SearchOutcome::Found(pkgs) => add_section(&mut output, &source.display_name(), pkgs, source.color()),
            SearchOutcome::TimedOut => output.push_str(&format!(
                "{}{}{}{} — timed out after {}s\n\n",
                BOLD, source.color(), source.display_name(), RESET, SEARCH_TIMEOUT.as_secs()
            )),
            SearchOutcome::Failed(msg) => output.push_str(&format!(
                "{}{}{}{} — search failed: {}\n\n",
                BOLD, source.color(), source.display_name(), RESET, msg
            )),
        }
    }

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

        let thumb_h = if total <= view_h {
            view_h
        } else {
            ((view_h * view_h) / total).max(1).min(view_h)
        };
        let track_h   = view_h.saturating_sub(thumb_h);
        let thumb_top = if track_h == 0 { 0 } else { offset * track_h / max_offset };

        for row in 0..view_h {
            execute!(stdout, cursor::MoveTo(0, row as u16))?;
            write!(stdout, "\x1B[0m\x1B[2K")?;
            if let Some(line) = lines.get(offset + row) {
                write!(stdout, "{}", line)?;
            }
        }

        for row in 0..view_h {
            execute!(stdout, cursor::MoveTo(scroll_col, row as u16))?;
            write!(stdout, "\x1B[0m")?;
            if row >= thumb_top && row < thumb_top + thumb_h {
                write!(stdout, "█")?;
            } else {
                write!(stdout, "{}│{}", DIM, RESET)?;
            }
        }

        execute!(stdout, cursor::MoveTo(0, rows - 1))?;
        write!(stdout,
            "\x1B[0m\x1B[7m  {}/{} lines  │  ↑↓ / PgUp PgDn / scroll  │  q to quit  \x1B[0m",
            (offset + 1).min(total), total
        )?;

        stdout.flush()?;

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
                    MouseEventKind::Up(_) => { dragging = false; continue; }
                    MouseEventKind::Down(_) if m.column == scroll_col => {
                        dragging = true;
                        let row = (m.row as usize).min(view_h.saturating_sub(1));
                        offset = if track_h == 0 { 0 }
                                 else { (row * max_offset / track_h).min(max_offset) };
                    }
                    MouseEventKind::Drag(_) if dragging => {
                        let row = (m.row as usize).min(view_h.saturating_sub(1));
                        offset = if track_h == 0 { 0 }
                                 else { (row * max_offset / track_h).min(max_offset) };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xbps_parses_state_pkgver_and_description() {
        let out = concat!(
            "[-] zsh-5.9_6                    A shell with lots of features\n",
            "[*] gtk-doc-1.33.2_1              Documentation generator for GTK\n",
        );
        let pkgs = parse_xbps_output(out);
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].name, "zsh");
        assert_eq!(pkgs[0].version, "5.9_6");
        assert_eq!(pkgs[0].description, "A shell with lots of features");
        assert_eq!(pkgs[1].name, "gtk-doc");
        assert_eq!(pkgs[1].version, "1.33.2_1");
    }

    #[test]
    fn apt_parses_name_dash_description() {
        let out = concat!(
            "vim - Vi IMproved - enhanced vi editor\n",
            "zsh - shell with lots of features\n",
        );
        let pkgs = parse_apt_output(out);
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].name, "vim");
        assert_eq!(pkgs[0].description, "Vi IMproved - enhanced vi editor");
        assert_eq!(pkgs[1].name, "zsh");
        assert_eq!(pkgs[1].description, "shell with lots of features");
    }

    #[test]
    fn yum_dnf_parses_name_arch_colon_description() {
        let out = concat!(
            "Last metadata expiration check: 0:34:12 ago.\n",
            "============ Name & Summary Matched: firefox ============\n",
            "firefox.x86_64 : Mozilla Firefox Web browser\n",
            "firefox-langpacks.noarch : Langpacks for firefox\n",
        );
        let pkgs = parse_yum_dnf_output(out);
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].name, "firefox");
        assert_eq!(pkgs[0].description, "Mozilla Firefox Web browser");
        assert_eq!(pkgs[1].name, "firefox-langpacks");
    }

    #[test]
    fn split_pkgver_handles_hyphenated_names() {
        assert_eq!(split_pkgver("xtools-6.3_1"), ("xtools".to_string(), "6.3_1".to_string()));
        assert_eq!(split_pkgver("gtk-doc-1.33.2_1"), ("gtk-doc".to_string(), "1.33.2_1".to_string()));
        assert_eq!(split_pkgver("noversion"), ("noversion".to_string(), "unknown".to_string()));
    }

    // run_and_capture's whole job is telling a real error apart from a
    // clean empty result without relying on exit status (pacman exits
    // non-zero for a normal "no matches"). These use `sh` to simulate each
    // stream/exit combination directly rather than depending on any
    // package manager being present in the test env.

    #[tokio::test]
    async fn run_and_capture_returns_stdout_on_clean_success() {
        let out = run_and_capture("sh", &["-c", "echo hello"]).await.unwrap();
        assert_eq!(out.trim(), "hello");
    }

    #[tokio::test]
    async fn run_and_capture_treats_silent_nonzero_exit_as_empty_not_failure() {
        // Mirrors pacman exiting 1 with nothing on either stream for a
        // normal "nothing matched" -- must NOT be reported as an error.
        let out = run_and_capture("sh", &["-c", "exit 1"]).await.unwrap();
        assert!(out.trim().is_empty());
    }

    #[tokio::test]
    async fn run_and_capture_surfaces_stderr_when_stdout_is_empty() {
        let err = run_and_capture("sh", &["-c", "echo 'boom: something broke' >&2; exit 1"])
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "boom: something broke");
    }

    #[tokio::test]
    async fn run_and_capture_ignores_stderr_when_stdout_has_content() {
        // A warning alongside real results shouldn't be treated as failure.
        let out = run_and_capture("sh", &["-c", "echo real-result; echo warning >&2"])
            .await
            .unwrap();
        assert_eq!(out.trim(), "real-result");
    }
}
