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
const YELLOW: &str = "\x1B[33m";
const MAGENTA: &str = "\x1B[35m";
const CYAN: &str = "\x1B[36m";
const BRIGHT_BLUE: &str = "\x1B[94m";
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
    maybe_bootstrap_nix().await;
    let results = search_all_sources(&search_term).await;

    if results.is_empty() {
        eprintln!("{}Error:{} no supported package manager found on this system.", RED, RESET);
        eprintln!("Supported: pacman, paru/yay, xbps-query, apt-cache, dnf/yum, nix, flatpak.");
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
    Nix,
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
            Source::Nix => "Nix".to_string(),
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
            Source::Nix => BRIGHT_BLUE,
            Source::Flatpak => GREEN,
        }
    }

    // Everything else here just queries an already-built local cache or
    // makes one quick network call. `nix search` is the outlier: the first
    // run on a given nixpkgs revision has to evaluate (or fetch and
    // evaluate) the package set to build its local search cache, which can
    // easily take well past 10s. Later searches against the same revision
    // are fast, since that cache is then warm.
    fn timeout(&self) -> std::time::Duration {
        match self {
            Source::Nix => std::time::Duration::from_secs(60),
            _ => std::time::Duration::from_secs(10),
        }
    }
}

// Probes the system for every backend we know how to talk to. All checks run
// concurrently; the fixed ordering of the pushes below (not completion order)
// determines the display order later, so output stays stable across runs.
async fn detect_sources() -> Vec<Source> {
    let (pacman, aur, xbps, apt, yum_dnf, nix, flatpak) = tokio::join!(
        command_exists("pacman"),
        detect_aur_helper(),
        command_exists("xbps-query"),
        command_exists("apt-cache"),
        detect_yum_dnf(),
        command_exists("nix"),
        command_exists("flatpak"),
    );

    let mut sources = Vec::new();
    if pacman { sources.push(Source::Pacman); }
    if let Some(helper) = aur { sources.push(Source::Aur(helper)); }
    if xbps { sources.push(Source::Xbps); }
    if apt { sources.push(Source::Apt); }
    if let Some(bin) = yum_dnf { sources.push(Source::YumDnf(bin)); }
    if nix { sources.push(Source::Nix); }
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

#[derive(Clone, Copy)]
enum Escalation {
    Sudo,
    Doas,
    Su,
}

impl Escalation {
    fn program(&self) -> &'static str {
        match self {
            Escalation::Sudo => "sudo",
            Escalation::Doas => "doas",
            Escalation::Su => "su",
        }
    }

    // sudo/doas exec the given argv directly, so a shell that understands
    // `&&` has to be named explicitly. `su -c` already passes its argument
    // through the target (root) shell on its own.
    fn args_for<'a>(&self, shell_command: &'a str) -> Vec<&'a str> {
        match self {
            Escalation::Su => vec!["-c", shell_command],
            Escalation::Sudo | Escalation::Doas => vec!["sh", "-c", shell_command],
        }
    }

    // How this would read as a one-liner in the prompt shown to the user.
    fn display(&self, shell_command: &str) -> String {
        match self {
            Escalation::Su => format!("su -c \"{}\"", shell_command),
            Escalation::Sudo | Escalation::Doas => {
                format!("{} sh -c \"{}\"", self.program(), shell_command)
            }
        }
    }
}

async fn detect_privilege_escalation() -> Option<Escalation> {
    if command_exists("sudo").await { Some(Escalation::Sudo) }
    else if command_exists("doas").await { Some(Escalation::Doas) }
    else if command_exists("su").await { Some(Escalation::Su) }
    else { None }
}

// $USER isn't always set -- minimal shells, some non-login sessions, and
// (as testing this turned up) some sandboxed/root environments all leave it
// empty even though the user is perfectly well-defined. whoami asks the
// kernel directly and is a much more reliable fallback.
async fn resolve_username() -> Option<String> {
    match std::env::var("USER") {
        Ok(u) if !u.is_empty() => Some(u),
        _ => resolve_username_via_whoami().await,
    }
}

async fn resolve_username_via_whoami() -> Option<String> {
    match run_and_capture("whoami", &[]).await {
        Ok(out) if !out.trim().is_empty() => Some(out.trim().to_string()),
        _ => None,
    }
}

// Real usernames are always in this set; this is a safety net for the
// (extremely unlikely) case of something odder, since the name gets
// interpolated into a shell command string below.
fn is_safe_username(user: &str) -> bool {
    !user.is_empty() && user.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

// Whether nix could actually create things directly under /nix right now.
// Existence alone isn't enough to tell -- distro nix packages often ship
// /nix as an empty, root-owned stub (expecting nix-daemon to sort out
// permissions later), which "exists" but is just as unusable as it being
// missing outright, and produces the exact same permission error either
// way. Actually probing beats stat()-ing the mode/owner and reasoning about
// it by hand, since it has to hold for whichever user is running this.
fn can_write_to_nix_dir() -> bool {
    let probe = std::path::Path::new("/nix/.pd-write-test");
    match std::fs::File::create(probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(probe);
            true
        }
        Err(_) => false,
    }
}

fn nix_dir_has_no_content() -> bool {
    match std::fs::read_dir("/nix") {
        Ok(mut entries) => entries.next().is_none(),
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

// If `nix` is on PATH but `/nix` isn't writable, every nix invocation fails
// trying to touch the store -- this is what you get after e.g. `pacman -S
// nix` without ever starting nix-daemon, which is normally what creates
// and owns /nix (sometimes that leaves /nix missing entirely, sometimes it
// leaves an empty root-owned stub -- either way nix can't use it). That's
// one clear cause with one safe fix, so offer to run it once, up front,
// rather than showing the same failure every time. Deliberately narrow:
// this only fires when /nix is missing or empty. If it already has real
// content in it, this doesn't guess -- taking ownership of a directory
// something else populated on purpose is a meaningfully riskier move than
// claiming an empty one, so that case is left alone and just shows
// whatever nix itself reports.
async fn maybe_bootstrap_nix() {
    if !command_exists("nix").await {
        return;
    }
    if can_write_to_nix_dir() {
        return;
    }
    if !nix_dir_has_no_content() {
        return;
    }
    let Some(user) = resolve_username().await else {
        return; // can't tell who to chown it to -- don't guess
    };
    if !is_safe_username(&user) {
        return;
    }

    eprintln!(
        "{}Nix{} is on PATH, but {}/nix{} isn't writable by you, so nix search will fail.",
        BOLD, RESET, BOLD, RESET
    );

    let command = format!("mkdir -p /nix && chown {}: /nix", user);

    let Some(escalation) = detect_privilege_escalation().await else {
        eprintln!("I couldn't find sudo, doas, or su to fix this automatically. As root:");
        eprintln!("  {}", command);
        return;
    };

    eprintln!("This can be fixed once with:");
    eprintln!("  {}", escalation.display(&command));
    eprint!("Run this now? [y/N] ");

    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() {
        return;
    }
    if !answer.trim().eq_ignore_ascii_case("y") {
        return;
    }

    // Inherits our stdin/stdout, so a password prompt from sudo/doas/su
    // shows up on the real terminal and can actually be answered.
    let status = TokioCommand::new(escalation.program())
        .args(escalation.args_for(&command))
        .status().await;

    match status.map(|s| s.success()) {
        Ok(true) => {}
        Ok(false) => eprintln!("{}Warning:{} that didn't succeed; nix may still fail below.", RED, RESET),
        Err(e) => eprintln!("{}Warning:{} couldn't run {}: {}", RED, RESET, escalation.program(), e),
    }
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
        let timeout = source.timeout();
        tokio::spawn(async move {
            let result = tokio::time::timeout(timeout, run_search(source, &term)).await;
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
        Source::Nix => search_nix(term).await,
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
// treat a non-zero exit as failure -- pacman (and, from what I recall, nix)
// both exit non-zero for a perfectly normal "nothing matched", so doing
// that would misreport a chunk of clean, empty results as errors. What IS a
// reliable signal, since a genuine "no matches" is normally silent on both
// streams, is stdout coming back empty while stderr has something in it --
// that combination means the tool actually had something to say.
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
// e.g. "[-] zsh-5.9_6   A shell with lots of features". Worth a quick sanity
// check against a live Void box -- this is from memory, not a test system.
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

async fn search_nix(term: &str) -> std::io::Result<Vec<Package>> {
    let stdout = run_and_capture(
        "nix",
        &["--extra-experimental-features", "nix-command flakes", "search", "nixpkgs", term],
    ).await?;
    Ok(parse_nix_output(&stdout))
}

// `nix search nixpkgs <term>` prints "* attr.path (version)" then an indented
// description line. Not every package has a description, so we peek before
// consuming the next line rather than assuming it's always there.
fn parse_nix_output(stdout: &str) -> Vec<Package> {
    let mut results = Vec::new();
    let mut lines = stdout.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(header) = line.strip_prefix("* ") else { continue };
        let (attr_path, version) = match header.rsplit_once(" (") {
            Some((path, ver)) => (path.trim(), ver.trim_end_matches(')').to_string()),
            None => (header.trim(), "unknown".to_string()),
        };
        let name = nix_pkg_name(attr_path);
        let description = match lines.peek() {
            Some(next) if !next.trim_start().starts_with("* ") && !next.trim().is_empty() => {
                lines.next().unwrap().trim().to_string()
            }
            _ => "No description.".to_string(),
        };
        results.push(Package::new(name, version, description));
    }
    results
}

fn nix_pkg_name(attr_path: &str) -> String {
    attr_path.rsplit('.').next().unwrap_or(attr_path).trim_matches('"').to_string()
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
                SearchOutcome::TimedOut => format!("timed out after {}s", source.timeout().as_secs()),
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
                BOLD, source.color(), source.display_name(), RESET, source.timeout().as_secs()
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
    fn nix_parses_header_and_optional_description() {
        let out = concat!(
            "* legacyPackages.x86_64-linux.hello (2.12.1)\n",
            "  A program that produces a familiar, friendly greeting\n",
            "* legacyPackages.x86_64-linux.undescribed (1.0)\n",
            "* legacyPackages.x86_64-linux.next (3.0)\n",
            "  Another package\n",
        );
        let pkgs = parse_nix_output(out);
        assert_eq!(pkgs.len(), 3);
        assert_eq!(pkgs[0].name, "hello");
        assert_eq!(pkgs[0].version, "2.12.1");
        assert!(pkgs[0].has_description());
        assert_eq!(pkgs[1].name, "undescribed");
        assert_eq!(pkgs[1].description, "No description.");
        assert_eq!(pkgs[2].name, "next");
        assert_eq!(pkgs[2].description, "Another package");
    }

    #[test]
    fn split_pkgver_handles_hyphenated_names() {
        assert_eq!(split_pkgver("xtools-6.3_1"), ("xtools".to_string(), "6.3_1".to_string()));
        assert_eq!(split_pkgver("gtk-doc-1.33.2_1"), ("gtk-doc".to_string(), "1.33.2_1".to_string()));
        assert_eq!(split_pkgver("noversion"), ("noversion".to_string(), "unknown".to_string()));
    }

    // run_and_capture's whole job is telling a real error apart from a
    // clean empty result without relying on exit status (which pacman and
    // nix both use non-zero for on a normal "no matches"). These use `sh`
    // to simulate each stream/exit combination directly rather than
    // depending on any package manager being present in the test env.

    #[tokio::test]
    async fn run_and_capture_returns_stdout_on_clean_success() {
        let out = run_and_capture("sh", &["-c", "echo hello"]).await.unwrap();
        assert_eq!(out.trim(), "hello");
    }

    #[tokio::test]
    async fn run_and_capture_treats_silent_nonzero_exit_as_empty_not_failure() {
        // Mirrors pacman/nix exiting 1 with nothing on either stream for a
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

    #[tokio::test]
    async fn resolve_username_via_whoami_returns_a_name() {
        // Regression test: this is the fallback maybe_bootstrap_nix needs
        // when $USER is unset, which turned out to be true in the sandbox
        // this was developed in despite it being a perfectly normal (root)
        // session -- relying on $USER alone silently broke the whole
        // feature. Deliberately doesn't touch $USER itself, since env vars
        // are process-global and cargo test runs tests in parallel.
        let user = resolve_username_via_whoami().await;
        assert!(user.as_deref().is_some_and(|u| !u.is_empty()));
    }

    #[test]
    fn is_safe_username_accepts_normal_names_rejects_shell_syntax() {
        assert!(is_safe_username("george"));
        assert!(is_safe_username("user_01"));
        assert!(is_safe_username("first-last"));
        assert!(!is_safe_username(""));
        assert!(!is_safe_username("george; rm -rf /"));
        assert!(!is_safe_username("$(whoami)"));
        assert!(!is_safe_username("has space"));
    }

    #[test]
    fn escalation_wraps_command_in_a_shell_for_sudo_and_doas_but_not_su() {
        let cmd = "mkdir -p /nix && chown george: /nix";
        assert_eq!(Escalation::Sudo.args_for(cmd), vec!["sh", "-c", cmd]);
        assert_eq!(Escalation::Doas.args_for(cmd), vec!["sh", "-c", cmd]);
        // su's own -c already passes the command through the root shell,
        // so it doesn't need (or want) an extra `sh -c` wrapper.
        assert_eq!(Escalation::Su.args_for(cmd), vec!["-c", cmd]);
    }

    #[test]
    fn escalation_display_matches_what_actually_runs() {
        let cmd = "mkdir -p /nix && chown george: /nix";
        assert_eq!(Escalation::Sudo.display(cmd), "sudo sh -c \"mkdir -p /nix && chown george: /nix\"");
        assert_eq!(Escalation::Su.display(cmd), "su -c \"mkdir -p /nix && chown george: /nix\"");
    }
}
