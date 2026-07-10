# Support selectable local development modes for custom env vars

## Context

PR #6 (https://github.com/cerby-engineering/dpcp/pull/6) added a flat `env:`
map to `dpcp.yml` for injecting custom environment variables into
`.dpcp.env`:

```yaml
env:
  key: value
  key: value
```

A later upgrade may want to support multiple selectable named modes instead
of a single flat map, so a working directory can switch between different
sets of custom env vars (e.g. developing against fully local services vs. a
hybrid setup that talks to cloud-hosted dependencies):

```yaml
envs:
  bare-local:
    key: value
  hybrid-local-cloud:
    key: value
```

This would presumably need some way to select the active mode (e.g. a CLI
arg to `dpcp allocate`, an env var, or a field in `dpcp.yml` naming the
default/active mode) since only one mode's vars should be written to
`.dpcp.env` at a time.

## Relevant files

- `src/main.rs` — `DpcpConfig.env` field (currently `BTreeMap<String, String>`) and the `cmd_allocate` writing logic that emits the `# Custom environment variables` block
- `README.md` — documents the current flat `env:` field under the `dpcp.yml` section

## Next steps

- Decide how the active mode gets selected (CLI flag vs. config field vs. env var)
- Decide whether `env:` (flat) and `envs:` (multi-mode) coexist or if this is a breaking change to `env:`
