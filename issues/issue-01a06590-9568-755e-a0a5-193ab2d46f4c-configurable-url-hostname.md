# Configurable hostname for emitted URLs

## Context

When dpcp runs on a remote machine that the user reaches over SSH (or
Tailscale), the URLs it prints and writes are hardcoded to loopback, so
they aren't clickable from the machine the user is actually sitting at.

Example: running dpcp on `earlye-herd-claude` prints
`http://127.0.0.1:8080/`, but the user needs
`http://earlye-herd-claude.raccoon-wyrm.ts.net:8080/`.

The ask is a way to supply a hostname — possibly per port, not just
globally — so the emitted URLs point at a reachable name.

Note there are currently **two different hardcoded hostnames** in the code,
which is itself an inconsistency this issue should probably resolve:

- terminal output (`allocate` and `list`) uses `127.0.0.1`
- the `<PREFIX>_URL` line written into `.dpcp.env` uses `localhost`

## Relevant files

- `src/main.rs:196` — `service_display()` formats `{scheme}://127.0.0.1:{port}/` for terminal output
- `src/main.rs:313` — `cmd_allocate` prints via `service_display`
- `src/main.rs:366` — `cmd_list` prints via `service_display`
- `src/main.rs:287` — `cmd_allocate` writes `{prefix}_URL={scheme}://localhost:{port}` into `.dpcp.env`
- `src/main.rs:7` — `DpcpConfig` struct; a global `hostname:` field would go here
- `src/main.rs:17` — `PortConfig` struct; a per-port `hostname:` override would go here
- `src/main.rs:31` — `ServiceRequest::from_spec` parses the `name:port[:scheme]` CLI form; a hostname would need a place in that grammar too
- `README.md` — documents `dpcp.yml` fields and shows the emitted URL format

## Decisions

**Scope: display only.** The configurable hostname changes what `dpcp allocate`
and `dpcp list` *print*. The `<PREFIX>_URL` var written to `.dpcp.env` is
untouched and keeps its current `localhost` form.

The two channels have different audiences and want different answers:

| Channel | Audience | Wants |
|---|---|---|
| Terminal output | A human, at a *different* machine over SSH | The reachable name |
| `<PREFIX>_URL` in `.dpcp.env` | Processes on the dpcp host (docker-compose, clients) | `localhost` |

Pointing `_URL` at a Tailscale name would route host-local traffic out and
back, and would break outright from inside a container or with tailscaled
down. Anyone who genuinely wants a non-loopback URL in their env file
already has `env:` interpolation (PR #7) for it.

Consequence: this is a pure presentation change. No sqlite schema change, no
new `.dpcp.env` var, and no risk to running services.

**Source: `DPCP_HOSTNAME` environment variable**, with a `--hostname` flag as
a per-invocation override. Unset means today's behavior, unchanged.

The hostname is a property of *the box dpcp runs on*, not of the project, so
it belongs at host level:

- `dpcp.yml` is a **committed repo file** (verified). A Tailscale name put
  there would ship one machine's identity to every clone; the same repo on a
  laptop wants no hostname at all.
- `cmd_list` renders allocations across all working directories and never
  opens any `dpcp.yml`. A host-level env var works for `list` and `allocate`
  identically, with no sqlite column and no new file format.

Set once per box:

```sh
# ~/.bashrc on the remote host
export DPCP_HOSTNAME=earlye-herd-claude.raccoon-wyrm.ts.net
```

Auto-detection (`tailscale status`, `hostname -f`) was considered and
rejected: on a multi-homed host there's no principled way to pick between the
LAN name, the Tailscale name and public DNS, and it's awkward to turn off.

## Open questions

- Is the existing `127.0.0.1` (display) vs `localhost` (env file) split
  intentional, or a bug to fold into this change?

## Related

- The `env:` interpolation feature (PR #6/#7) already lets a user hand-write a URL with a fixed hostname, e.g. `WEBAPP_LOCAL_URL: http://app.cerby-local.com:${WEBAPP_PORT}/` — that's the current workaround, and overlaps with what this issue proposes to make first-class.

## Grill Log

### 2026-09-03

- Q: Should the configurable hostname change `.dpcp.env`'s `_URL` vars, or only terminal output? — A: **Display only.** `_URL` stays `localhost`; the two channels have different audiences, and `env:` interpolation already covers the case for a custom URL in the env file.
- Q: Where should the display hostname be configured? — A: **`DPCP_HOSTNAME` env var**, plus a `--hostname` per-invocation override. It's host-level, not project-level: `dpcp.yml` is committed to git, and `dpcp list` never reads it. Auto-detection rejected as ambiguous on multi-homed hosts.
