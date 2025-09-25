use std::io::Write;
use std::process::{Command, Stdio};
use std::env;
use std::collections::HashMap;
use tokio::process::Command as TokioCommand;

// ANSI color codes
const BOLD: &str = "\x1B[1m";
const BLUE: &str = "\x1B[34m";
const RED: &str = "\x1B[31m";
const GREEN: &str = "\x1B[32m";
const RESET: &str = "\x1B[0m";

#[derive(Clone, Debug)]
struct Package {
    name: String,
    version: String,
    description: String,
}

impl Package {
    fn new(name: String, version: String, description: String) -> Self {
        Self { name, version, description }
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

async fn search_all_sources(term: &str) -> Result<(Vec<Package>, Vec<Package>, Vec<Package>), Box<dyn std::error::Error>> {
    let timeout = std::time::Duration::from_secs(10);
    
    let (pacman_result, aur_result, flatpak_result) = tokio::join!(
        tokio::time::timeout(timeout, search_pacman(term)),
        tokio::time::timeout(timeout, search_aur(term)),
        tokio::time::timeout(timeout, search_flatpak(term))
    );

    let pacman = handle_search_result(pacman_result, "Pacman");
    let aur = handle_search_result(aur_result, "AUR");
    let flatpak = handle_search_result(flatpak_result, "Flatpak");

    Ok((pacman, aur, flatpak))
}

fn handle_search_result(
    result: Result<Result<Vec<Package>, std::io::Error>, tokio::time::error::Elapsed>,
    source: &str
) -> Vec<Package> {
    match result {
        Ok(Ok(packages)) => dedupe_packages(packages),
        Ok(Err(e)) => {
            eprintln!("{}Warning:{} {} search failed: {}", RED, RESET, source, e);
            Vec::new()
        }
        Err(_) => {
            eprintln!("{}Warning:{} {} search timed out", RED, RESET, source);
            Vec::new()
        }
    }
}

fn dedupe_packages(mut packages: Vec<Package>) -> Vec<Package> {
    if packages.len() <= 1 {
        return packages;
    }
    
    let mut seen = HashMap::new();
    packages.retain(|pkg| {
        match seen.get(&pkg.name) {
            Some(existing) if is_better_package(pkg, existing) => {
                seen.insert(pkg.name.clone(), pkg.clone());
                false // Remove the current one, we'll add the better one
            }
            Some(_) => false, // Keep existing, remove current
            None => {
                seen.insert(pkg.name.clone(), pkg.clone());
                true
            }
        }
    });
    
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    packages
}

fn is_better_package(new: &Package, existing: &Package) -> bool {
    let new_has_desc = !new.description.is_empty() && new.description != "No description.";
    let existing_has_desc = !existing.description.is_empty() && existing.description != "No description.";
    
    match (new_has_desc, existing_has_desc) {
        (true, false) => true,
        (false, true) => false,
        _ => new.version.len() > existing.version.len() || 
             (new.version.len() == existing.version.len() && new.version > existing.version)
    }
}

async fn search_pacman(term: &str) -> std::io::Result<Vec<Package>> {
    let output = TokioCommand::new("pacman")
        .args(["-Ss", term])
        .output()
        .await?;
    
    if !output.status.success() {
        return Ok(Vec::new());
    }
    
    parse_pacman_output(&String::from_utf8_lossy(&output.stdout))
}

fn parse_pacman_output(stdout: &str) -> std::io::Result<Vec<Package>> {
    let mut results = Vec::new();
    let mut lines = stdout.lines();
    
    while let Some(line) = lines.next() {
        if let Some(package) = parse_pacman_line(line, &mut lines) {
            results.push(package);
        }
    }
    
    Ok(results)
}

fn parse_pacman_line(line: &str, lines: &mut std::str::Lines) -> Option<Package> {
    if !line.contains('/') {
        return None;
    }
    
    let parts: Vec<&str> = line.splitn(2, '/').collect();
    let name_version = parts.get(1)?;
    
    let mut nv_parts = name_version.splitn(2, ' ');
    let name = nv_parts.next()?.trim().to_string();
    let version_part = nv_parts.next()?.trim();
    
    let version = extract_version(version_part);
    let description = lines.next()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_string())
        .unwrap_or_else(|| "No description.".to_string());
    
    Some(Package::new(name, version, description))
}

async fn search_aur(term: &str) -> std::io::Result<Vec<Package>> {
    let helper = if command_exists("paru").await {
        "paru"
    } else if command_exists("yay").await {
        "yay"
    } else {
        return Ok(Vec::new());
    };

    let output = TokioCommand::new(helper)
        .args(["-Ss", "--aur", term])
        .output()
        .await?;
    
    if !output.status.success() {
        return Ok(Vec::new());
    }
    
    parse_aur_output(&String::from_utf8_lossy(&output.stdout))
}

fn parse_aur_output(stdout: &str) -> std::io::Result<Vec<Package>> {
    let mut results = Vec::new();
    let mut lines = stdout.lines();
    
    while let Some(line) = lines.next() {
        if line.contains("aur/") {
            if let Some(package) = parse_aur_line(line, &mut lines) {
                results.push(package);
            }
        }
    }
    
    Ok(results)
}

fn parse_aur_line(line: &str, lines: &mut std::str::Lines) -> Option<Package> {
    let parts: Vec<&str> = line.splitn(2, '/').collect();
    let name_version = parts.get(1)?;
    
    let mut nv_parts = name_version.splitn(2, ' ');
    let name = nv_parts.next()?.trim().to_string();
    let version_part = nv_parts.next()?.trim();
    
    let version = extract_version(version_part);
    let description = lines.next()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_string())
        .unwrap_or_else(|| "No description.".to_string());
    
    Some(Package::new(name, version, description))
}

async fn search_flatpak(term: &str) -> std::io::Result<Vec<Package>> {
    if !command_exists("flatpak").await {
        return Ok(Vec::new());
    }

    let output = TokioCommand::new("flatpak")
        .args(["search", "--columns=name,application,version,description", term])
        .output()
        .await?;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    parse_flatpak_output(&String::from_utf8_lossy(&output.stdout), term)
}

fn parse_flatpak_output(stdout: &str, term: &str) -> std::io::Result<Vec<Package>> {
    let term_lower = term.to_lowercase();
    let mut results = Vec::new();
    
    for line in stdout.lines().skip(1) { // Skip header
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 4 {
            continue;
        }
        
        let name = parts[0].trim();
        if !name.to_lowercase().contains(&term_lower) {
            continue;
        }
        
        let app_id = parts[1].trim();
        let version = if parts[2].trim().is_empty() { "Unknown" } else { parts[2].trim() };
        let description = if parts[3].trim().is_empty() { "No description." } else { parts[3].trim() };
        
        results.push(Package::new(
            format!("{} ({})", name, app_id),
            version.to_string(),
            description.to_string(),
        ));
    }
    
    Ok(results)
}

fn extract_version(version_part: &str) -> String {
    version_part.find('(')
        .and_then(|start| version_part.find(')').map(|end| (start, end)))
        .and_then(|(start, end)| {
            if start < end && start + 1 < version_part.len() {
                Some(version_part[start + 1..end].to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| version_part.to_string())
}

async fn command_exists(cmd: &str) -> bool {
    TokioCommand::new("which")
        .arg(cmd)
        .output()
        .await
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn print_results(pacman: &[Package], aur: &[Package], flatpak: &[Package]) {
    if pacman.is_empty() && aur.is_empty() && flatpak.is_empty() {
        println!("No packages found.");
        return;
    }
    
    let mut output = format!(
        "{}Pacman:{} {} | {}AUR:{} {} | {}Flatpak:{} {}\n\n",
        BOLD, RESET, format_count(pacman.len()),
        BOLD, RESET, format_count(aur.len()),
        BOLD, RESET, format_count(flatpak.len())
    );

    add_section(&mut output, "Pacman", pacman, BLUE);
    add_section(&mut output, "AUR", aur, RED);
    add_section(&mut output, "Flatpak", flatpak, GREEN);

    if should_use_pager(&output) {
        use_pager(&output);
    } else {
        print!("{}", output);
    }
}

fn format_count(count: usize) -> String {
    if count == 1 { "1 package".to_string() } else { format!("{} packages", count) }
}

fn add_section(output: &mut String, name: &str, packages: &[Package], color: &str) {
    if packages.is_empty() {
        return;
    }
    
    output.push_str(&format!("{}{} Results:{}\n", BOLD, name, RESET));
    output.push_str(&format!("{}\n", "=".repeat(name.len() + 9)));
    
    for pkg in packages {
        output.push_str(&format!("{}{}{}{}\n", BOLD, color, pkg.name, RESET));
        output.push_str(&format!("  {}\n", pkg.description));
        output.push_str(&format!("  {}Version:{} {}\n\n", BOLD, RESET, pkg.version));
    }
}

fn should_use_pager(output: &str) -> bool {
    let lines = output.matches('\n').count();
    let height = get_terminal_height().unwrap_or(24);
    lines > height - 2 && command_exists_sync("less")
}

fn use_pager(content: &str) {
    if let Ok(mut pager) = Command::new("less")
        .args(["-R", "+Gg"])
        .stdin(Stdio::piped())
        .spawn()
    {
        if let Some(mut stdin) = pager.stdin.take() {
            let _ = stdin.write_all(content.as_bytes());
        }
        let _ = pager.wait();
    } else {
        print!("{}", content);
    }
}

fn command_exists_sync(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn get_terminal_height() -> Option<usize> {
    let output = Command::new("stty")
        .arg("size")
        .stderr(Stdio::null())
        .output()
        .ok()?;
        
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}
