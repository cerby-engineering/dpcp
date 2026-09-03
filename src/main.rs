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
    #[serde(default)]
    env: std::collections::BTreeMap<String, String>,
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

fn load_dpcp_yml(dir: &Path) -> Result<(Vec<ServiceRequest>, Option<PathBuf>, std::collections::BTreeMap<String, String>)> {
    let cwd = dir;
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

    Ok((requests, env_file, config.env))
}

#[derive(Parser)]
#[command(name = "dpcp", about = "Dynamic Port Configuration Protocol — host-central port broker for working directories")]
struct Cli {
    /// Hostname to use in printed URLs, e.g. a Tailscale name when dpcp runs on a
    /// remote box. Defaults to $DPCP_HOSTNAME; pass an empty value to ignore it.
    #[arg(long, global = true)]
    hostname: Option<String>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Allocate ports for a working directory and write .dpcp.env
    Allocate {
        /// Absolute path to the working directory
        workdir: PathBuf,
        /// Services with their default ports and optional scheme, e.g. postgres:5432 web:3000:http.
        /// If omitted, reads from dpcp.yml in the working directory.
        services: Vec<String>,
        /// Where to write the env file (defaults to dpcp.yml env-file, then <workdir>/.dpcp.env)
        #[arg(long)]
        env_file: Option<PathBuf>,
    },
    /// Release all port allocations for a working directory
    Release {
        /// Absolute path to the working directory
        workdir: PathBuf,
    },
    /// List all current allocations, optionally filtered by glob (use '.' for current working directory)
    List {
        /// Glob pattern to filter working directories, e.g. '*dpcp*' or '.' for current directory
        glob: Option<String>,
    },
    /// Remove allocations for working directories whose paths no longer exist
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
            workdir  TEXT NOT NULL,
            service  TEXT NOT NULL,
            port     INTEGER NOT NULL,
            PRIMARY KEY (workdir, service)
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_port ON allocations(port);",
    )?;
    // idempotent migrations
    let _ = conn.execute_batch("ALTER TABLE allocations RENAME COLUMN worktree TO workdir");
    let _ = conn.execute_batch("ALTER TABLE allocations ADD COLUMN scheme TEXT");
    Ok(conn)
}

fn port_is_bound(port: u16) -> bool {
    use std::io::ErrorKind;
    use std::net::TcpListener;
    // Rust's std sets SO_REUSEADDR on Unix TcpListeners, which lets a wildcard bind
    // (0.0.0.0 / ::) coexist with a bind already held on a specific address
    // (127.0.0.1 / ::1), and vice versa. So a squatter on a specific loopback
    // address is only caught by probing that address directly, not the wildcard.
    let in_use = |res: std::io::Result<TcpListener>| {
        matches!(res, Err(e) if e.kind() == ErrorKind::AddrInUse)
    };
    in_use(TcpListener::bind(("0.0.0.0", port)))
        || in_use(TcpListener::bind(("127.0.0.1", port)))
        || in_use(TcpListener::bind(("::", port)))
        || in_use(TcpListener::bind(("::1", port)))
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

/// Render an allocation for the terminal. `hostname` is the display host from
/// `--hostname`/`$DPCP_HOSTNAME`; without one, URLs fall back to loopback and
/// port-only services stay bare numbers.
fn service_display(port: u16, scheme: Option<&str>, hostname: Option<&str>) -> String {
    match (scheme, hostname) {
        (Some(s @ ("http" | "https")), host) => {
            format!("{s}://{}:{port}/", host.unwrap_or("127.0.0.1"))
        }
        (_, Some(host)) => format!("{host}:{port}"),
        (_, None) => port.to_string(),
    }
}

/// Require a display hostname to be a bare host. dpcp supplies the scheme, port
/// and trailing slash itself, so `http://foo:3001` would render as
/// `http://http://foo:3001:8080/`. Characters outside the host charset are
/// rejected too: `http://foo?x:8080/` is a valid URL that a terminal linkifies
/// to host `foo` on port 80, which fails silently rather than loudly.
fn validate_hostname(host: &str) -> Result<()> {
    if host.contains("://") {
        anyhow::bail!("hostname must be a bare host, not a URL: {host}");
    }
    // A bracketed IPv6 literal is the one form allowed to contain colons; it
    // also renders correctly as-is, since `[::1]:8080` is the standard form.
    if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        let ipv6ish = |c: char| c.is_ascii_hexdigit() || c == ':' || c == '.';
        if inner.is_empty() || !inner.chars().all(ipv6ish) {
            anyhow::bail!("hostname is not a valid bracketed IPv6 literal: {host}");
        }
        return Ok(());
    }
    if host.contains(':') {
        anyhow::bail!("hostname must not include a port (bracket IPv6 as [::1]): {host}");
    }
    let host_char = |c: char| c.is_ascii_alphanumeric() || c == '.' || c == '-';
    if !host.chars().all(host_char) {
        anyhow::bail!("hostname may only contain letters, digits, '.' and '-': {host}");
    }
    Ok(())
}

/// Resolve the display hostname from the `--hostname` flag, else
/// `$DPCP_HOSTNAME`. An empty or whitespace-only value means "no hostname",
/// so `--hostname ""` cancels an inherited `$DPCP_HOSTNAME`.
fn resolve_hostname(flag: Option<String>) -> Result<Option<String>> {
    let raw = flag.or_else(|| std::env::var("DPCP_HOSTNAME").ok());
    let host = match raw {
        Some(h) if !h.trim().is_empty() => h.trim().to_string(),
        _ => return Ok(None),
    };
    validate_hostname(&host)?;
    Ok(Some(host))
}

/// Substitute `${NAME}` references in `value` with entries from `lookup`.
/// Values with no `${...}` are returned unchanged. Errors on an unterminated
/// `${` or a reference to a name that isn't in `lookup`.
fn interpolate_env_value(value: &str, lookup: &HashMap<String, String>) -> Result<String> {
    let mut result = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        result.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .with_context(|| format!("unterminated \"${{\" in custom env value: {value}"))?;
        let name = &after[..end];
        let resolved = lookup.get(name).with_context(|| {
            format!("custom env value references unknown variable \"${{{name}}}\": {value}")
        })?;
        result.push_str(resolved);
        rest = &after[end + 1..];
    }
    result.push_str(rest);
    Ok(result)
}

fn cmd_allocate(
    workdir: &Path,
    requests: &[ServiceRequest],
    env_file: Option<&Path>,
    extra_env: &std::collections::BTreeMap<String, String>,
    hostname: Option<&str>,
) -> Result<()> {
    let workdir = workdir
        .canonicalize()
        .with_context(|| format!("working directory path does not exist: {}", workdir.display()))?;
    let workdir_str = workdir.to_string_lossy();

    let conn = open_db()?;

    let mut assignments: HashMap<String, u16> = HashMap::new();

    for req in requests {
        let existing: Option<u16> = conn
            .query_row(
                "SELECT port FROM allocations WHERE workdir = ?1 AND service = ?2",
                params![workdir_str.as_ref(), req.name],
                |row| row.get(0),
            )
            .ok();

        let port = if let Some(p) = existing {
            conn.execute(
                "UPDATE allocations SET scheme = ?1 WHERE workdir = ?2 AND service = ?3",
                params![req.scheme.as_deref(), workdir_str.as_ref(), req.name],
            )?;
            p
        } else {
            let p = next_free_port(&conn, req.default_port)?;
            conn.execute(
                "INSERT INTO allocations (workdir, service, port, scheme) VALUES (?1, ?2, ?3, ?4)",
                params![workdir_str.as_ref(), req.name, p, req.scheme.as_deref()],
            )?;
            p
        };
        assignments.insert(req.name.clone(), port);
    }

    let env_path = env_file
        .map(PathBuf::from)
        .unwrap_or_else(|| workdir.join(".dpcp.env"));

    let mut lines: Vec<String> = vec![
        "# Generated by dpcp — do not edit by hand".to_string(),
        format!("# Working directory: {workdir_str}"),
    ];
    let mut var_lookup: HashMap<String, String> = HashMap::new();
    for req in requests {
        let port = assignments[&req.name];
        let prefix = req.env_prefix();
        lines.push(format!("{prefix}_PORT={port}"));
        var_lookup.insert(format!("{prefix}_PORT"), port.to_string());
        if req.default_port != port {
            lines.push(format!(
                "# ^ {} default {}, allocated {port}",
                req.name, req.default_port
            ));
        }
        if matches!(req.scheme.as_deref(), Some("http" | "https")) {
            let scheme = req.scheme.as_deref().unwrap();
            let url = format!("{scheme}://127.0.0.1:{port}");
            lines.push(format!("{prefix}_URL={url}"));
            var_lookup.insert(format!("{prefix}_URL"), url);
        }
    }

    if !extra_env.is_empty() {
        lines.push(String::new());
        lines.push("# Custom environment variables".to_string());
        for (key, value) in extra_env {
            let interpolated = interpolate_env_value(value, &var_lookup)
                .with_context(|| format!("failed to interpolate custom env var \"{key}\""))?;
            lines.push(format!("{key}={interpolated}"));
        }
    }

    lines.push(String::new());

    std::fs::write(&env_path, lines.join("\n"))
        .with_context(|| format!("failed to write {}", env_path.display()))?;

    println!("{workdir_str}");
    for req in requests {
        let port = assignments[&req.name];
        println!("  {}: {}", req.name, service_display(port, req.scheme.as_deref(), hostname));
    }

    Ok(())
}

fn cmd_release(workdir: &Path) -> Result<()> {
    // Canonicalize if the path still exists; fall back to the raw path for already-deleted working directories.
    let canonical = workdir.canonicalize().unwrap_or_else(|_| workdir.to_path_buf());
    let workdir_str = canonical.to_string_lossy();
    let conn = open_db()?;
    let deleted = conn.execute(
        "DELETE FROM allocations WHERE workdir = ?1",
        params![workdir_str.as_ref()],
    )?;
    println!("Released {deleted} allocation(s) for {workdir_str}");
    Ok(())
}

fn cmd_list(glob: Option<&str>, hostname: Option<&str>) -> Result<()> {
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
        "SELECT workdir, service, port, scheme FROM allocations ORDER BY workdir, service",
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
    for (workdir, service, port, scheme) in &filtered {
        if *workdir != current_wt {
            println!("{workdir}");
            current_wt = workdir.clone();
        }
        println!("  {service}: {}", service_display(*port, scheme.as_deref(), hostname));
    }
    Ok(())
}

fn cmd_gc() -> Result<()> {
    let conn = open_db()?;
    let mut stmt =
        conn.prepare("SELECT DISTINCT workdir FROM allocations")?;
    let workdirs: Vec<String> = stmt
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;

    let mut freed = 0usize;
    for wt in workdirs {
        if !Path::new(&wt).exists() {
            let n = conn.execute(
                "DELETE FROM allocations WHERE workdir = ?1",
                params![wt],
            )?;
            println!("GC: removed {n} allocation(s) for missing working directory {wt}");
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
        Commands::Allocate { workdir, services, env_file } => {
            let (requests, yml_env_file, extra_env) = if services.is_empty() {
                load_dpcp_yml(&workdir).context("no services given and failed to load dpcp.yml")?
            } else {
                let reqs = services.iter()
                    .map(|s| ServiceRequest::from_spec(s))
                    .collect::<Result<Vec<_>>>()?;
                (reqs, None, std::collections::BTreeMap::new())
            };
            let effective_env_file = env_file.as_deref().or(yml_env_file.as_deref());
            let hostname = resolve_hostname(cli.hostname)?;
            let hostname = hostname.as_deref();
            cmd_allocate(&workdir, &requests, effective_env_file, &extra_env, hostname)
        }
        Commands::Release { workdir } => cmd_release(&workdir),
        Commands::List { glob } => {
            let hostname = resolve_hostname(cli.hostname)?;
            cmd_list(glob.as_deref(), hostname.as_deref())
        }
        Commands::Gc => cmd_gc(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolate_passthrough_when_no_reference() {
        let lookup = HashMap::new();
        assert_eq!(
            interpolate_env_value("plain-value", &lookup).unwrap(),
            "plain-value"
        );
    }

    #[test]
    fn interpolate_substitutes_known_reference() {
        let mut lookup = HashMap::new();
        lookup.insert("WEBAPP_PORT".to_string(), "3001".to_string());
        assert_eq!(
            interpolate_env_value("http://app.cerby-local.com:${WEBAPP_PORT}/", &lookup).unwrap(),
            "http://app.cerby-local.com:3001/"
        );
    }

    #[test]
    fn interpolate_supports_multiple_references() {
        let mut lookup = HashMap::new();
        lookup.insert("A".to_string(), "1".to_string());
        lookup.insert("B".to_string(), "2".to_string());
        assert_eq!(
            interpolate_env_value("${A}-${B}", &lookup).unwrap(),
            "1-2"
        );
    }

    #[test]
    fn interpolate_errors_on_unknown_reference() {
        let lookup = HashMap::new();
        assert!(interpolate_env_value("${TYPO}", &lookup).is_err());
    }

    #[test]
    fn interpolate_errors_on_unterminated_brace() {
        let lookup = HashMap::new();
        assert!(interpolate_env_value("${OOPS", &lookup).is_err());
    }

    #[test]
    fn service_display_covers_scheme_and_hostname_combinations() {
        let host = Some("box.ts.net");
        let cases = [
            (Some("http"), host, "http://box.ts.net:8080/"),
            (Some("https"), host, "https://box.ts.net:8080/"),
            (Some("http"), None, "http://127.0.0.1:8080/"),
            (None, host, "box.ts.net:8080"),
            (None, None, "8080"),
        ];
        for (scheme, hostname, expected) in cases {
            assert_eq!(service_display(8080, scheme, hostname), expected);
        }
    }

    #[test]
    fn validate_hostname_accepts_bare_hosts_and_bracketed_ipv6() {
        assert!(validate_hostname("foo.ts.net").is_ok());
        assert!(validate_hostname("my-box").is_ok());
        assert!(validate_hostname("[::1]").is_ok());
        assert!(validate_hostname("[fd7a:115c:a1e0::1]").is_ok());
    }

    #[test]
    fn validate_hostname_rejects_anything_that_would_corrupt_a_url() {
        for bad in [
            "http://foo", // scheme
            "foo:3001",   // port
            "foo/bar",    // path
            "foo?x",      // query — would linkify as host `foo` on port 80
            "foo#y",      // fragment
            "my box",     // whitespace
            "[]",         // empty IPv6 literal
            "[nope]",     // not hex
        ] {
            assert!(validate_hostname(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn resolve_hostname_treats_empty_flag_as_unset() {
        assert_eq!(resolve_hostname(Some(String::new())).unwrap(), None);
        assert_eq!(resolve_hostname(Some("   ".to_string())).unwrap(), None);
        assert_eq!(
            resolve_hostname(Some(" foo.ts.net ".to_string())).unwrap(),
            Some("foo.ts.net".to_string())
        );
    }
}
