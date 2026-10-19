# CLI-0128: a local Grafana/Prometheus/Loki stack for bare-metal pigeon-cli runs

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-11.
- **Status**: Accepted.

## Context

Previous iterations of this project ran `pigeon` transcoding/inference-stage
jobs on Scaleway cloud compute, provisioned by the sibling `noisypigeon`
terraform repo, and observed via Scaleway's managed "Cockpit" product
(Grafana + Mimir/Prometheus + Loki, CLI-0092/0093). That pipeline worked by
cloud-initing a Grafana Alloy agent onto each compute instance
(`noisypigeon`'s `compute-instance` module): Alloy scrapes `pigeon`'s own
`127.0.0.1:9091/metrics` endpoint every 15s and tails `pigeon.jsonl`,
pushing both to Cockpit via `remote_write`/`loki.write`. One Grafana
dashboard ("pigeon-cli overview", provisioned via Terraform from
`noisypigeon`'s `workloads/.../shared/grafana/dashboards/overview.json")
renders job-phase, CPU/mem/disk, upload-throughput, and log panels from
that data.

Cloud compute is now cost-prohibitive for the transcode/inference stage,
so job runs are shifting back to bare-metal on the user's own Mac. This
drops observability entirely: the Alloy agent only exists on Scaleway
instances, so nothing scrapes `/metrics` or tails `pigeon.jsonl` locally.
Both CLI-0093 and the `analyze-job-run` skill already say this plainly —
that data "is simply gone once the process exits" on a machine with no
Alloy/Cockpit wired up.

Crucially, `pigeon-cli` needs **no source changes** to fix this. It
already exposes exactly what a local observability stack needs:

1. A loopback-only (`127.0.0.1`), pull-based, process-lifetime Prometheus
   endpoint (`src/observability/metrics.rs`, default port `9091`, override
   via `--metrics-port`/`PIGEON_METRICS_PORT`, disable via `--no-metrics`),
   emitting `pigeon_resource_cpu_percent`/`mem_bytes`/`disk_{read,written}_bytes_total`,
   `pigeon_job_phase_total{pigeon_job,phase,outcome,instance,source_bucket}`,
   `pigeon_job_macro_phase`, the `pigeon_upload_*` family, and
   `pigeon_command_*` — all labeled `instance` = a cached hostname
   (CLI-0092/0093/0097).
2. A structured JSONL log at `pigeon.jsonl` (default macOS path
   `~/Library/Application Support/pigeon/logs/pigeon.jsonl`), each line
   carrying top-level `timestamp` and `spans[0].command`/`spans[0].instance`
   fields (CLI-0073/0093).

This ADR adds a Docker Compose stack, living in this repo, that plays the
same role Alloy+Cockpit played in the cloud — scraping/tailing those same
two surfaces into a local Prometheus + Loki + Grafana setup — while
`pigeon` itself keeps running bare-metal, unchanged. It deliberately does
not attempt cloud/local config unification or a return-to-cloud story;
it's a standalone local dev convenience, living in this repo rather than
`noisypigeon` because `noisypigeon`'s `terraform/` tree is a strict
Scaleway/Terragrunt convention (CLI-0098/`root.hcl`) that a local
docker-compose stack doesn't fit, and because it sits right next to the
metrics/log producer it consumes, mirroring CLI-0087's Dockerfile
precedent.

## Decision

### 1. New `observability/` directory

Sibling to the existing `Dockerfile`/`.dockerignore` from CLI-0087:

```
observability/
├── docker-compose.yml
├── prometheus/prometheus.yml
├── promtail/promtail-config.yml
└── grafana/
    ├── provisioning/datasources/datasources.yml
    ├── provisioning/dashboards/dashboards.yml
    └── dashboards/pigeon-cli-overview.json
```

Four services: `prometheus`, `loki`, `promtail`, `grafana`. Named volumes
`prom-data`/`grafana-data` (mirrors CLI-0087's `pigeon-data` volume
convention) so history survives `docker compose down`/`up`.

### 2. Prometheus scrapes pigeon's existing endpoint

```yaml
# prometheus/prometheus.yml
scrape_configs:
  - job_name: pigeon_cli
    honor_labels: true
    static_configs:
      - targets: ["host.docker.internal:9091"]
```

`honor_labels: true` lets pigeon's own `instance` label (a real hostname)
win over the scrape-target-derived one Prometheus would otherwise assign
— the same relabeling the cloud-side Alloy agent did explicitly.

Docker Desktop for Mac's `host.docker.internal` is expected to reach a
host service bound to `127.0.0.1` (the common pattern for reaching e.g. a
host-local Postgres) — this is an assumption to verify empirically on
first real setup, not yet proven in this exact environment. If it does
**not** work, the fallback is running Prometheus outside Docker (e.g.
`brew install prometheus`, pointed at the same `prometheus.yml`) — **not**
rebinding pigeon's metrics listener to `0.0.0.0`. That bind address is a
deliberate CLI-0092 security decision; reversing it needs its own
follow-up ADR with its own threat-model discussion, not a reflexive fix
folded into this one.

### 3. Promtail tails `pigeon.jsonl`

Chosen over reusing Grafana Alloy: Prometheus already does the scrape
half here, so Alloy would only be used for its log-tailing third, while
Promtail is the purpose-built, narrower tool for exactly that job, with
`pipeline_stages` that port Alloy's `stage.json`/`stage.timestamp`/
`stage.labels` almost 1:1.

```yaml
# promtail/promtail-config.yml (pipeline excerpt)
pipeline_stages:
  - json:
      expressions:
        ts: timestamp
        pigeon_job: spans[0].command
        instance: spans[0].instance
  - timestamp:
      source: ts
      format: RFC3339Nano
  - labels:
      pigeon_job:
      instance:
```

Bind-mounted read-only from pigeon's default macOS log directory:

```yaml
# docker-compose.yml, promtail service
volumes:
  - "${HOME}/Library/Application Support/pigeon/logs:/var/log/pigeon:ro"
```

If `PIGEON_LOG_DIR`/`--log-file` is customized, only this bind-mount's
host-side path needs to change to match.

Known inherited gap (true of the cloud Alloy pipeline today too, not
introduced here): the one auto-emitted span-close event per run has an
empty `spans[]`, so that single line per run lands in Loki without
`pigeon_job`/`instance` labels. Cosmetic — documented, not "fixed."

### 4. Grafana: provisioned datasources + adapted dashboard

Datasources and the dashboard provider are both file-provisioned (no
manual click-ops setup needed on `docker compose up`):

```yaml
# grafana/provisioning/datasources/datasources.yml
datasources:
  - name: Prometheus
    type: prometheus
    url: http://prometheus:9090
    uid: prometheus
    isDefault: true
  - name: Loki
    type: loki
    url: http://loki:3100
    uid: loki
```

`grafana/dashboards/pigeon-cli-overview.json` is adapted from
`noisypigeon`'s `workloads/willowgraysen.com/terraform/pigeon-cli/shared/grafana/dashboards/overview.json`,
with these changes:

- A **stable `"uid"`** (`"pigeon-cli-overview"`, was `null`) — required
  for idempotent re-provisioning on every `docker compose up`; otherwise
  Grafana creates a duplicate dashboard each restart.
- The `prometheus`/`loki` template variables' `"current"` now point at the
  provisioned `uid`s directly (file-based provisioning has no
  Terraform-side injection step like the cloud version had).
- **Panel 4 ("Disk space utilization") is dropped entirely** — it's 100%
  `node_exporter`-sourced (`node_filesystem_*`), and v1 deliberately skips
  a macOS host-wide exporter (see Out of scope).
- The `instance` template variable's query changes from
  `label_values(node_cpu_seconds_total, instance)` (node_exporter-only,
  would show no choices locally) to
  `label_values(pigeon_resource_cpu_percent, instance)` (pigeon's own
  metric, sampled every 5s whenever a `pigeon` process is running).
- Panels 1 (job phases), 2 (CPU), 3 (memory), 5 (disk I/O), 6 (upload
  throughput), 7 (logs) port over verbatim — already 100%
  pigeon-emitted-metric/log-sourced.

### 5. mise tasks

```toml
[tasks.observability-up]
run = "docker compose -f observability/docker-compose.yml up -d"

[tasks.observability-down]
run = "docker compose -f observability/docker-compose.yml down"

[tasks.observability-logs]
run = "docker compose -f observability/docker-compose.yml logs -f \"$@\""
```

Matches CLI-0087's `docker-build`/`docker-run` task shape exactly.

### 6. README.md

A new section, sibling to the existing "Run via Docker" section, covers
starting the stack, opening Grafana (`http://localhost:3000`, default
`admin`/`admin` — change it), running `pigeon` normally alongside it, and
the one documented v1 limitation (no host-wide metrics).

## Consequences

- Metrics+logs+dashboard parity with the previous cloud setup is restored
  locally, with **zero `pigeon-cli` source changes** — `src/observability/`
  and `src/cli.rs`'s existing flags/env-vars are consumed exactly as they
  already exist today.
- A new local infra surface to maintain: 4 containers, their own
  images/updates/disk usage (Prometheus TSDB + Grafana state in named
  volumes).
- The `host.docker.internal` → loopback-bound-host-port reachability
  assumption (§2) is not yet empirically confirmed as of this ADR landing
  — verify it on first real setup; the documented fallback (host-installed
  Prometheus) does not touch pigeon's bind address.
- Prometheus/Grafana/Loki are long-lived containers, independent of any
  single `pigeon` invocation's lifetime — a target/dashboard panel
  dropping to `up==0`/showing a gap between runs is expected, not an
  outage, matching the posture CLI-0092 already established for the cloud
  setup.
- This stack is unmanaged by Terraform/`noisypigeon` — purely a local dev
  convenience, started/stopped manually via `mise` tasks, with no
  persistence/backup/HA story beyond the named Docker volumes.

## Out of scope

- node_exporter or any macOS host-wide metrics exporter — v1 relies
  solely on pigeon's own self-reported `pigeon_resource_*` process-tree
  metrics; whole-machine CPU/mem/disk-free visibility (the one thing
  panel 4 provided in the cloud dashboard) is a known, accepted gap, not
  silently dropped.
- Alerting rules of any kind.
- Publishing/registry concerns for the new container images — they're
  pulled directly from upstream Docker Hub/Grafana registries, not built
  or republished by this repo.
- Any `pigeon-cli` source code change.
- Managing this stack via the `noisypigeon` terraform repo or any CI
  pipeline — it's a manual, locally-gated `mise`-task-driven stack, same
  posture as CLI-0087's `docker-build`/`docker-run` tasks.
- Reconciling this with a future return to cloud compute, if/when cost
  allows — this ADR doesn't attempt to make the local and cloud
  observability setups share config or tooling.

Implementation lands in the same PR as this ADR.
