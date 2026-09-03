# Host-level settings live outside `dpcp.yml`

`dpcp.yml` is committed to the repository, so it can only hold facts true of
the *project* — a machine-specific value put there would ship one host's
identity to every clone. Settings scoped to the box dpcp runs on therefore
come from the environment instead, starting with `DPCP_HOSTNAME` (the
hostname used when rendering allocations for display).

A second, independent reason points the same way: `cmd_list` renders
allocations across every known working directory by reading `~/.dpcp.sqlite`,
and never opens any `dpcp.yml`. A project-file setting would be invisible to
`dpcp list` unless it were also persisted as a database column.

## Consequences

- The test for where a new setting belongs is *whose fact is it?* — the
  project's (`dpcp.yml`) or the host's (environment). It is not a question of
  convenience.
- Host-level settings need no sqlite column to be honored by `dpcp list`.
- Unset means today's behavior. A host that configures nothing behaves
  exactly as before, which keeps the same repo working unchanged on a laptop
  and on a remote box.

## Considered and rejected

- **A `hostname:` field in `dpcp.yml`** — commits a machine-specific name
  into shared source, and is invisible to `dpcp list`.
- **Auto-detection** (`tailscale status`, `hostname -f`) — on a multi-homed
  host there is no principled way to choose between the LAN name, the
  Tailscale name and public DNS, and it is awkward to turn off.
- **A host-level config file** (`~/.dpcp.toml`) — a reasonable future home if
  host-level settings multiply, but not worth a new file format and parse
  path for a single string. The boundary this ADR draws is between *project*
  and *host*; which host-level mechanism carries the value is a smaller,
  reversible choice.
