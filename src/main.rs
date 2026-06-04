use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rusqlite::{Connection, params};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(serde::Deserialize)]
struct DpcpConfig {
    #[serde(rename = "env-file")]
    env_file: Option<PathBuf>,
    ports: HashMap<String, PortConfig>,
}

#[derive(serde::Deserialize)]
struct PortConfig {
    #[serde(rename = "default-port")]
    default_port: u16,
    protocol: Option<String>,
    #[serde(rename = "env-name")]
    env_name: Option<String>,
}

struct ServiceRequest {
    name: String,
    default_port: u16,
    scheme: Option<String>,
    env_name: Option<String>,
}

impl ServiceRequest {
    fn from_spec(spec: &str) -> Result<Self> {
        let parts: Vec<&str> = spec.splitn(3, ':').collect();
        let (name, port_str, scheme) = match parts.as_slice() {
            [n, p] => (*n, *p, None),
            [n, p, s] => (*n, *p, Some(s.to_string())),
            _ => anyhow::bail!("expected service:port[:scheme], got '{spec}'"),
        };
        Ok(ServiceRequest {
            name: name.to_string(),
            default_port: port_str.parse().with_context(|| format!("invalid port in '{spec}'"))?,
            scheme,
            env_name: None,
        })
    }

    fn env_prefix(&self) -> String {
        self.env_name.as_deref().unwrap_or(&self.name).to_uppercase().replace('-', "_")
    }
}

fn load_dpcp_yml() -> Result<(Vec<ServiceRequest>, Option<PathBuf>)> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let path = cwd.join("dpcp.yml");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let config: DpcpConfig = serde_yaml::from_str(&text)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    let mut requests: Vec<ServiceRequest> = config.ports.into_iter()
        .map(|(name, pc)| ServiceRequest {
            name,
            default_port: pc.default_port,
            scheme: pc.protocol,
            env_name: pc.env_name,
        })
        .collect();
    requests.sort_by(|a, b| a.name.cmp(&b.name));

    let env_file = config.env_file.map(|p| if p.is_absolute() { p } else { cwd.join(p) });

    Ok((requests, env_file))
}

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
        /// Services with their default ports and optional scheme, e.g. postgres:5432 web:3000:http.
        /// If omitted, reads from dpcp.yml in the current directory.
        services: Vec<String>,
        /// Where to write the env file (defaults to dpcp.yml env-file, then <worktree>/.dpcp.env)
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

fn service_display(port: u16, scheme: Option<&str>) -> String {
    match scheme {
        Some(s @ ("http" | "https")) => format!("{s}://127.0.0.1:{port}/"),
        _ => port.to_string(),
    }
}

fn cmd_allocate(
    worktree: &Path,
    requests: &[ServiceRequest],
    env_file: Option<&Path>,
) -> Result<()> {
    let worktree = worktree
        .canonicalize()
        .with_context(|| format!("worktree path does not exist: {}", worktree.display()))?;
    let worktree_str = worktree.to_string_lossy();

    let conn = open_db()?;

    let mut assignments: HashMap<String, u16> = HashMap::new();

    for req in requests {
        let existing: Option<u16> = conn
            .query_row(
                "SELECT port FROM allocations WHERE worktree = ?1 AND service = ?2",
                params![worktree_str.as_ref(), req.name],
                |row| row.get(0),
            )
            .ok();

        let port = if let Some(p) = existing {
            conn.execute(
                "UPDATE allocations SET scheme = ?1 WHERE worktree = ?2 AND service = ?3",
                params![req.scheme.as_deref(), worktree_str.as_ref(), req.name],
            )?;
            p
        } else {
            let p = next_free_port(&conn, req.default_port)?;
            conn.execute(
                "INSERT INTO allocations (worktree, service, port, scheme) VALUES (?1, ?2, ?3, ?4)",
                params![worktree_str.as_ref(), req.name, p, req.scheme.as_deref()],
            )?;
            p
        };
        assignments.insert(req.name.clone(), port);
    }

    let env_path = env_file
        .map(PathBuf::from)
        .unwrap_or_else(|| worktree.join(".dpcp.env"));

    let mut lines: Vec<String> = vec![
        "# Generated by dpcp — do not edit by hand".to_string(),
        format!("# Worktree: {worktree_str}"),
    ];
    for req in requests {
        let port = assignments[&req.name];
        let prefix = req.env_prefix();
        lines.push(format!("{prefix}_PORT={port}"));
        if req.default_port != port {
            lines.push(format!(
                "# ^ {} default {}, allocated {port}",
                req.name, req.default_port
            ));
        }
        if matches!(req.scheme.as_deref(), Some("http" | "https")) {
            let scheme = req.scheme.as_deref().unwrap();
            lines.push(format!("{prefix}_URL={scheme}://localhost:{port}"));
        }
    }
    lines.push(String::new());

    std::fs::write(&env_path, lines.join("\n"))
        .with_context(|| format!("failed to write {}", env_path.display()))?;

    println!("{worktree_str}");
    for req in requests {
        let port = assignments[&req.name];
        println!("  {}: {}", req.name, service_display(port, req.scheme.as_deref()));
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
        println!("  {service}: {}", service_display(*port, scheme.as_deref()));
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
            let (requests, yml_env_file) = if services.is_empty() {
                load_dpcp_yml().context("no services given and failed to load dpcp.yml")?
            } else {
                let reqs = services.iter()
                    .map(|s| ServiceRequest::from_spec(s))
                    .collect::<Result<Vec<_>>>()?;
                (reqs, None)
            };
            let effective_env_file = env_file.as_deref().or(yml_env_file.as_deref());
            cmd_allocate(&worktree, &requests, effective_env_file)
        }
        Commands::Release { worktree } => cmd_release(&worktree),
        Commands::List { glob } => cmd_list(glob.as_deref()),
        Commands::Gc => cmd_gc(),
    }
}
