# ADR 0083 — Compose profiles: surface opt-in services instead of hiding them

Date: 2026-09-16
Status: accepted; implemented.

## Context

The per-folder services popover is built from `docker compose ps
--all`: it shows containers that exist. A service gated behind a
compose `profiles:` list (moon-landing's `moongit-index`, `shell`,
`rabbitmq`) is skipped by profile-less `up`, so its container never
exists and the service is simply invisible — a dev who just added a
profiled service to their branch's compose sees nothing and can't
tell whether moon-ide failed to parse the file or the service is
opt-in. The same blind spot hides _any_ declared-but-never-created
service. And a profiled container the user started by hand escaped
project-wide teardown: `down` without the profile leaves it running
as an orphan.

## Decision

Merge the compose config's full service list into the `ps` snapshot;
keep profiles opt-in for starting, all-inclusive for teardown.

- **Enumeration.** `ProjectCompose::status()` runs
  `docker compose --profile "*" config --format json` (the `"*"`
  wildcard needs compose ≥ 2.24; on failure the lister falls back to
  the plain profile-less `config --services`, i.e. exactly the old
  behaviour) and folds the result into the `ps --all` rows: existing
  containers get their service's `profiles` list attached; declared
  services with no container are appended as synthetic
  `raw_state: "absent"` rows. `ServiceStatus` gains `profiles:
Vec<String>` and the `"absent"` sentinel.
- **Aggregate state ignores absent rows.** It's computed from the
  `ps` rows before the merge — a stack whose profiled services were
  never started is still `Running`, an empty daemon still `Absent`.
- **Starting stays opt-in.** Project-wide `up` / `rebuild` pass no
  profile flag. The per-service "▶" works on absent rows unchanged:
  explicitly targeting a service auto-activates its profiles
  (documented compose behaviour), and `up -d --no-deps <svc>` is
  already the row's verb.
- **Teardown is all-profiles.** `stop` / `down` / `pause` /
  `unpause` pass `--profile "*"` — but only when the config actually
  declares a profiled service, so profile-less projects (and older
  compose that can't resolve the wildcard, detected via the lister's
  fallback) keep byte-identical invocations.
- **Restart override covers profiled services.** The generated
  `restart: "no"` override now lists them too — inert while the
  profile is inactive (the merge keeps the base file's `profiles:`),
  effective the moment one is started from the popover.
- **UI.** Absent rows render muted (`not created`, hollow dot) with
  a profile chip and only the "▶" affordance (no logs — nothing to
  stream). The chip also appears on running profiled containers.
  Companion mirrors the same, minimally.

## Rejected alternatives

- **Activate profiles on project-wide `up`.** Defeats the author's
  intent — moon-landing marks these opt-in precisely because most
  devs shouldn't run them.
- **Hand-parse the compose YAML for profiles.** Same reason the
  restart override shells out to `config`: `extends:`, `include:`,
  anchors, env interpolation. Compose's own resolver or nothing.
- **A per-folder "enabled profiles" setting.** No second concrete
  need yet (ADR 0006 posture); explicit per-service start covers the
  observed workflow. Revisit if someone wants a profile brought up
  as a group.
- **Cache the config probe by file mtime.** The status probe is
  already TTL-cached, and every lifecycle op runs an identical-cost
  `config` round-trip for the restart override today; a second cache
  layer is complexity without a measured need.

## Related

- [ADR 0017 — project compose restart override](0017-project-compose-restart-override.md)
  — the `config --services` round-trip this extends with profiles.
- [ADR 0020 — container status cache](0020-container-status-cache.md)
  — the TTL cache the extra config call rides.
- [`specs/containers.md` § Compose profiles](../containers.md#compose-profiles-adr-0083).
