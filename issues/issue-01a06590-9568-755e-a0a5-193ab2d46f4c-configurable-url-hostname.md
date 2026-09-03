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

## Open questions

- Global default hostname, per-port override, or both?
- Config-file field, CLI flag, environment variable, or auto-detect (e.g. hostname of the box / Tailscale name)?
- Does the hostname affect only display, or also the `_URL` var written to `.dpcp.env` (which downstream services consume)? These may want to differ — a container may need `localhost` while a human needs the Tailscale name.
- Is the existing `127.0.0.1` vs `localhost` split intentional, or a bug to fold into this change?
- Should the hostname be persisted in the sqlite allocation row (like `scheme` is), so `dpcp list` can render it for working directories whose `dpcp.yml` isn't re-read?

## Related

- The `env:` interpolation feature (PR #6/#7) already lets a user hand-write a URL with a fixed hostname, e.g. `WEBAPP_LOCAL_URL: http://app.cerby-local.com:${WEBAPP_PORT}/` — that's the current workaround, and overlaps with what this issue proposes to make first-class.
