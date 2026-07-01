# dpcp allocates ports already bound on IPv6 (::1)

## Context

dpcp is not properly avoiding port allocations. A node process was squatting on
`[::1]:3000`, but dpcp allocated port 3000 anyway instead of skipping it.

## Root cause hypothesis

`port_is_bound` in `src/main.rs` only checks IPv4 by binding to `("0.0.0.0", port)`.
A process bound only to the IPv6 loopback address (`[::1]:3000`) does not conflict
with an IPv4 `0.0.0.0` bind attempt, so the probe succeeds and the port is
incorrectly reported as free.

## Relevant files

- `src/main.rs:162-165` — `port_is_bound`, only probes `TcpListener::bind(("0.0.0.0", port))` (IPv4), missing an IPv6 check
- `src/main.rs:167-181` — `next_free_port`, calls `port_is_bound` to decide whether a candidate port is available
