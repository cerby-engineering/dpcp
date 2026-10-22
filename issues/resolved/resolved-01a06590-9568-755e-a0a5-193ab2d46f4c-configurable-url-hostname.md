# Configurable hostname for emitted URLs

## Context

When dpcp runs on a remote machine the user reaches over SSH (or Tailscale),
the URLs it prints are hardcoded to loopback, so they aren't clickable from
the machine the user is actually sitting at.

Example: running dpcp on a remote box prints
`http://127.0.0.1:8080/`, but the user needs
`http://my-box.example.ts.net:8080/`.

Grilled 2026-09-03; the design below is settled. See
[ADR 0001](../docs/adr/0001-host-level-config-outside-dpcp-yml.md) for the
project-file vs. host-config boundary this establishes.

## Design

### Scope: display only

The hostname changes what `dpcp allocate` and `dpcp list` *print*. The
`<PREFIX>_URL` var written to `.dpcp.env` never carries it.

The two channels have different audiences and want different answers:

| Channel | Audience | Wants |
|---|---|---|
| Terminal output | A human, at a *different* machine over SSH | The reachable name |
| `<PREFIX>_URL` in `.dpcp.env` | Processes on the dpcp host (docker-compose, clients) | Loopback |

Pointing `_URL` at a Tailscale name would route host-local traffic out and
back, and would break outright from inside a container or with tailscaled
down. Anyone who genuinely wants a non-loopback URL in their env file already
has `env:` interpolation (PR #7) for it.

This keeps the change purely presentational: no sqlite schema change, no new
`.dpcp.env` var, no risk to running services.

### Source: `DPCP_HOSTNAME`

A `DPCP_HOSTNAME` environment variable, with a `--hostname` flag as a
per-invocation override. Unset means today's behavior, unchanged.

```sh
# ~/.bashrc on the remote host
export DPCP_HOSTNAME=my-box.example.ts.net
```

The hostname is a property of *the box dpcp runs on*, not of the project:

- `dpcp.yml` is a **committed repo file** (verified). A Tailscale name put
  there would ship one machine's identity to every clone; the same repo on a
  laptop wants no hostname at all.
- `cmd_list` renders allocations across all working directories and never
  opens any `dpcp.yml`. A host-level env var works for `list` and `allocate`
  identically, with no sqlite column and no new file format.

Auto-detection (`tailscale status`, `hostname -f`) was considered and
rejected: on a multi-homed host there's no principled way to pick between the
LAN name, the Tailscale name and public DNS, and it's awkward to turn off.

### Bare-port services are included

A service with no `protocol:` renders as `hostname:port` when a hostname is
set, so a `psql`/`redis-cli` connection string can be copied straight out:

```
$ dpcp list
/home/me/myproject
  webapp: http://my-box.example.ts.net:3001/
  postgres: my-box.example.ts.net:5432
```

With no hostname set they keep printing a bare integer (`5432`), not
`127.0.0.1:5432`. Non-HTTP services have no `_URL` var in `.dpcp.env`, only
`_PORT`, so nothing changes there.

### Folded in: normalize `.dpcp.env` to `127.0.0.1`

Separate from the remote hostname — just the loopback literal, so both
channels agree on their default. `.dpcp.env` currently writes
`http://localhost:3001` while display uses `127.0.0.1`.

`port_is_bound` already documents (`src/main.rs:167`) that IPv4 and IPv6
loopback are distinct addresses, and probes all four of `0.0.0.0`,
`127.0.0.1`, `::`, `::1` because a squatter on one isn't caught via the
other. Writing `localhost` reintroduces exactly that ambiguity — it resolves
to `::1` or `127.0.0.1` by resolver order, and a service bound IPv4-only
won't answer on `::1`.

The *trailing slash* difference (`…:8080/` printed, `…:3001` in the env file)
stays — it's justified on both sides: the slash helps terminals linkify, and
its absence lets consumers write `${WEBAPP_URL}/api/foo` without a double
slash.

### Dropped: per-port hostnames (YAGNI)

One hostname applies to every service. Analysis kept in case it returns:

No case surfaced for two services on one box wanting *different* names. The
nearest real case is the opposite — a service not reachable remotely at all
(an `http` admin panel bound loopback-only), where printing the Tailscale URL
would be a lie. That's a per-port **opt-out**, not a per-port hostname, and
it would belong in `dpcp.yml` ("binds loopback only" is a project fact,
unlike the box's name). Its cost is a new sqlite column, since `cmd_list`
reads the database rather than any `dpcp.yml` — exactly the schema change the
display-only scope avoids. Not worth it without a concrete instance.

## Implementation sketch

`service_display` grows a hostname parameter and a four-way match:

| `protocol:` | hostname | output |
|---|---|---|
| `http`/`https` | set | `{scheme}://{host}:{port}/` |
| `http`/`https` | unset | `{scheme}://127.0.0.1:{port}/` (unchanged) |
| absent | set | `{host}:{port}` |
| absent | unset | `{port}` (unchanged) |

- `src/main.rs:196` — `service_display()`: add the parameter and the match arms
- `src/main.rs:313`, `src/main.rs:366` — `cmd_allocate` / `cmd_list` call sites pass the resolved hostname
- `src/main.rs:287` — change the `_URL` line from `localhost` to `127.0.0.1`
- `src/main.rs:74` — `Cli`: add `--hostname` as a clap **global** arg so both `allocate` and `list` accept it. Precedence: `--hostname` > `DPCP_HOSTNAME` > none
- `src/main.rs:419` — existing `#[cfg(test)]` module: add table-driven tests for the four `service_display` cases
- `README.md` — document `DPCP_HOSTNAME` / `--hostname`, and update the `protocol:` row, which currently documents the output as `http://127.0.0.1:<port>/`

`ServiceRequest::from_spec` (`src/main.rs:31`, the `name:port[:scheme]` CLI
form) needs **no** change — the hostname is not per-service.

### Minor, resolved during implementation

- `--hostname ""` does cancel an inherited `DPCP_HOSTNAME`.
- Values are validated against a host charset allowlist, not a blocklist, and
  a bad value **warns and falls back** rather than failing — a display-only
  setting must not stop `dpcp allocate` from allocating.

### Added during implementation, not in the original design

Five rounds of self-review surfaced these; all are recorded in the commit
messages on the branch:

- Bare IPv6 addresses are bracketed automatically, so
  `--hostname "$(tailscale ip -6)"` works as written.
- Rejected as unreachable: wildcard binds (`0.0.0.0`, `::`), hyphen-edged
  labels (RFC 1123), empty labels, and hosts with no alphanumeric character.
- `_` is allowed in the charset (compose aliases, `/etc/hosts` entries).
- Version bumped to 0.2.0 to signal the `_URL` break.

### Declined

- Per-port hostnames (YAGNI, as decided in the grill).
- Leaving a bare IPv6 unbracketed in the `host:port` arm so it pastes into
  `psql -h`: that renders `fd7a::1:5432`, where the port isn't separable
  from the address.
- Erroring on `--hostname` for `release`/`gc`, which ignore it.

## Found but not fixed (out of scope)

`env-name` is treated as a *prefix*, not a name: `env-name: MY_SVC_PORT` on
an `http` service emits `MY_SVC_PORT_PORT` and `MY_SVC_PORT_URL`, while
`README.md` documents `MY_SVC_PORT`. Pre-existing (`env_prefix()`), verified
against the built binary, and untouched by this change — worth its own issue.

## Related

- The `env:` interpolation feature (PR #6/#7) already lets a user hand-write a
  URL with a fixed hostname, e.g.
  `WEBAPP_LOCAL_URL: http://app.cerby-local.com:${WEBAPP_PORT}/` — the current
  workaround, and the reason `_URL` doesn't need to carry the hostname itself.
- `issues/issue-019f4c83-…-selectable-env-modes.md` — proposes adding more to
  `dpcp.yml`; ADR 0001's project-vs-host boundary applies there too.

## Grill Log

### 2026-09-03

- Q: Should the configurable hostname change `.dpcp.env`'s `_URL` vars, or only terminal output? — A: **Display only.** `_URL` stays `localhost`; the two channels have different audiences, and `env:` interpolation already covers the case for a custom URL in the env file.
- Q: Where should the display hostname be configured? — A: **`DPCP_HOSTNAME` env var**, plus a `--hostname` per-invocation override. It's host-level, not project-level: `dpcp.yml` is committed to git, and `dpcp list` never reads it. Auto-detection rejected as ambiguous on multi-homed hosts.
- Q: Does "possibly per port" survive the display-only scope? — A: **Dropped, YAGNI** — might return, but no concrete case yet. The real near-case is a per-port *opt-out* for loopback-only services, which would cost a sqlite column because `cmd_list` reads the DB, not `dpcp.yml`.
- Q: Fix the `127.0.0.1` vs `localhost` split too? — A: **Yes** — change `.dpcp.env`'s `_URL` to `127.0.0.1`, removing the `::1` resolution ambiguity that `port_is_bound` already guards against. Trailing-slash difference stays; it's justified on both sides.
- Q: Should `DPCP_HOSTNAME` affect services with no `protocol:` (currently bare port numbers)? — A: **Yes**, render them as `hostname:port` so the connection string is copyable. (Overrode the recommendation to leave them bare.) Unset still prints a bare integer.
