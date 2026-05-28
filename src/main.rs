use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rusqlite::{Connection, params};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "dpcp", about = "Dynamic Port Configuration Protocol — host-central port broker for git worktrees")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Allocate ports for a worktree and write .dpcp.env
    Allocate {
        /// Absolute path to the worktree
        worktree: PathBuf,
        /// Services with their default ports and optional scheme, e.g. postgres:5432 web:3000:http
        #[arg(required = true)]
        services: Vec<String>,
        /// Where to write the env file (defaults to <worktree>/.dpcp.env)
        #[arg(long)]
        env_file: Option<PathBuf>,
    },
    /// Release all port allocations for a worktree
    Release {
        /// Absolute path to the worktree
        worktree: PathBuf,
    },
    /// List all current allocations, optionally filtered by glob (use '.' for current worktree)
    List {
        /// Glob pattern to filter worktrees, e.g. '*dpcp*' or '.' for current directory
        glob: Option<String>,
    },
    /// Remove allocations for worktrees whose paths no longer exist
    Gc,
}

/// Match a glob pattern (only `*` wildcard supported) against a string.
fn glob_match(pattern: &str, s: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == s;
    }
    let mut pos = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            if !s.starts_with(part) {
                return false;
            }
            pos = part.len();
        } else if i == parts.len() - 1 {
            if !s[pos..].ends_with(part) {
                return false;
            }
        } else {
            match s[pos..].find(part) {
                Some(idx) => pos += idx + part.len(),
                None => return false,
            }
        }
    }
    true
}

fn db_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".dpcp.sqlite")
}

fn open_db() -> Result<Connection> {
    let path = db_path();
    let conn = Connection::open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS allocations (
            worktree TEXT NOT NULL,
            service  TEXT NOT NULL,
            port     INTEGER NOT NULL,
            PRIMARY KEY (worktree, service)
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_port ON allocations(port);",
    )?;
    // idempotent migration: add scheme column if not present
    let _ = conn.execute_batch("ALTER TABLE allocations ADD COLUMN scheme TEXT");
    Ok(conn)
}

fn port_is_bound(port: u16) -> bool {
    use std::net::TcpListener;
    TcpListener::bind(("0.0.0.0", port)).is_err()
}

/// Find the lowest port >= start_port not in the dpcp database and not bound on the host.
fn next_free_port(conn: &Connection, start_port: u16) -> Result<u16> {
    let mut port = start_port;
    loop {
        let in_db: bool = conn.query_row(
            "SELECT COUNT(*) > 0 FROM allocations WHERE port = ?1",
            params![port],
            |row| row.get(0),
        )?;
        if !in_db && !port_is_bound(port) {
            return Ok(port);
        }
        port = port.checked_add(1).context("port space exhausted")?;
    }
}

fn cmd_allocate(
    worktree: &Path,
    services: &[String],
    env_file: Option<&Path>,
) -> Result<()> {
    let worktree = worktree
        .canonicalize()
        .with_context(|| format!("worktree path does not exist: {}", worktree.display()))?;
    let worktree_str = worktree.to_string_lossy();

    let conn = open_db()?;

    // Parse service:port[:scheme] triples
    let mut requests: Vec<(String, u16, Option<String>)> = Vec::new();
    for spec in services {
        let parts: Vec<&str> = spec.splitn(3, ':').collect();
        let (name, port_str, scheme) = match parts.as_slice() {
            [n, p] => (*n, *p, None),
            [n, p, s] => (*n, *p, Some(s.to_string())),
            _ => anyhow::bail!("expected service:port[:scheme], got '{spec}'"),
        };
        let default_port: u16 = port_str
            .parse()
            .with_context(|| format!("invalid port in '{spec}'"))?;
        requests.push((name.to_string(), default_port, scheme));
    }

    let mut assignments: HashMap<String, u16> = HashMap::new();

    for (service, default_port, scheme) in &requests {
        // Re-use existing allocation for this worktree+service if present
        let existing: Option<u16> = conn
            .query_row(
                "SELECT port FROM allocations WHERE worktree = ?1 AND service = ?2",
                params![worktree_str.as_ref(), service],
                |row| row.get(0),
            )
            .ok();

        let port = if let Some(p) = existing {
            // Update scheme in case it changed
            conn.execute(
                "UPDATE allocations SET scheme = ?1 WHERE worktree = ?2 AND service = ?3",
                params![scheme.as_deref(), worktree_str.as_ref(), service],
            )?;
            p
        } else {
            let p = next_free_port(&conn, *default_port)?;
            conn.execute(
                "INSERT INTO allocations (worktree, service, port, scheme) VALUES (?1, ?2, ?3, ?4)",
                params![worktree_str.as_ref(), service, p, scheme.as_deref()],
            )?;
            p
        };
        assignments.insert(service.clone(), port);
    }

    // Write env file
    let env_path = env_file
        .map(PathBuf::from)
        .unwrap_or_else(|| worktree.join(".dpcp.env"));

    let mut lines: Vec<String> = vec![
        "# Generated by dpcp — do not edit by hand".to_string(),
        format!("# Worktree: {worktree_str}"),
    ];
    for (service, default_port, scheme) in &requests {
        let port = assignments[service];
        let key = format!("{}_PORT", service.to_uppercase().replace('-', "_"));
        lines.push(format!("{key}={port}"));
        if *default_port != port {
            lines.push(format!(
                "# ^ {service} default {default_port}, allocated {port}"
            ));
        }
        if let Some(s) = scheme {
            let url_key = format!("{}_URL", service.to_uppercase().replace('-', "_"));
            lines.push(format!("{url_key}={s}://localhost:{port}"));
        }
    }
    lines.push(String::new());

    std::fs::write(&env_path, lines.join("\n"))
        .with_context(|| format!("failed to write {}", env_path.display()))?;

    println!("Wrote {}", env_path.display());
    for (service, default_port, _scheme) in &requests {
        let port = assignments[service];
        let flag = if *default_port == port { "" } else { " (reassigned)" };
        println!("  {service}: {port}{flag}");
    }

    Ok(())
}

fn cmd_release(worktree: &Path) -> Result<()> {
    // Canonicalize if the path still exists; fall back to the raw path for already-deleted worktrees.
    let canonical = worktree.canonicalize().unwrap_or_else(|_| worktree.to_path_buf());
    let worktree_str = canonical.to_string_lossy();
    let conn = open_db()?;
    let deleted = conn.execute(
        "DELETE FROM allocations WHERE worktree = ?1",
        params![worktree_str.as_ref()],
    )?;
    println!("Released {deleted} allocation(s) for {worktree_str}");
    Ok(())
}

fn cmd_list(glob: Option<&str>) -> Result<()> {
    // Resolve '.' to the canonical current directory path
    let resolved_glob: Option<String> = match glob {
        Some(".") => {
            let cwd = std::env::current_dir().context("failed to get current directory")?
                .canonicalize().context("failed to canonicalize current directory")?;
            Some(cwd.to_string_lossy().into_owned())
        }
        other => other.map(str::to_string),
    };

    let conn = open_db()?;
    let mut stmt = conn.prepare(
        "SELECT worktree, service, port, scheme FROM allocations ORDER BY worktree, service",
    )?;
    let rows: Vec<(String, String, u16, Option<String>)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let filtered: Vec<_> = match &resolved_glob {
        None => rows.iter().collect(),
        Some(pattern) => rows.iter().filter(|(wt, ..)| glob_match(pattern, wt)).collect(),
    };

    if filtered.is_empty() {
        println!("No allocations.");
        return Ok(());
    }

    let mut current_wt = String::new();
    for (worktree, service, port, scheme) in &filtered {
        if *worktree != current_wt {
            println!("{worktree}");
            current_wt = worktree.clone();
        }
        let display = match scheme.as_deref() {
            Some("http") => format!("http://127.0.0.1:{port}/"),
            _ => port.to_string(),
        };
        println!("  {service}: {display}");
    }
    Ok(())
}

fn cmd_gc() -> Result<()> {
    let conn = open_db()?;
    let mut stmt =
        conn.prepare("SELECT DISTINCT worktree FROM allocations")?;
    let worktrees: Vec<String> = stmt
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;

    let mut freed = 0usize;
    for wt in worktrees {
        if !Path::new(&wt).exists() {
            let n = conn.execute(
                "DELETE FROM allocations WHERE worktree = ?1",
                params![wt],
            )?;
            println!("GC: removed {n} allocation(s) for missing worktree {wt}");
            freed += n;
        }
    }
    if freed == 0 {
        println!("Nothing to collect.");
    } else {
        println!("Freed {freed} total allocation(s).");
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Allocate { worktree, services, env_file } => {
            cmd_allocate(&worktree, &services, env_file.as_deref())
        }
        Commands::Release { worktree } => cmd_release(&worktree),
        Commands::List { glob } => cmd_list(glob.as_deref()),
        Commands::Gc => cmd_gc(),
    }
}
