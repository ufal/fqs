# FQS (Rust)

This folder contains the Rust implementation of FQS used by Flexicorp/TEITOK workflows.

Current capabilities:

- native CLI executable (`fqs`)
- SQLite-backed corpus catalog (`fqs.db` by default)
- `corpora add` / `corpora list` / `corpora show` commands
- `query` command with live execution for `pando` corpora (via `flexicorp-pando` CLI)
- `corpora validate` command (basic + full probe mode)
- reindex control-plane and execution lifecycle:
  - `fqs reindex enqueue|queue|history|mark-started|mark-finished`
  - `fqs reindex dispatch-once` and `fqs reindex worker-heartbeat`
  - SQLite tables `reindex_jobs` + `reindex_history`
  - SQLite table `reindex_workers` (heartbeat/capacity)
  - HTTP routes `GET/POST /reindex/jobs`, `GET /reindex/history`,
    `POST /reindex/workers/heartbeat`, `POST /reindex/jobs/mark-started`,
    `POST /reindex/jobs/mark-finished`

A lightweight test HTTP mode is included (`--test`). Query execution is currently implemented for `pando` and `cqp`.

For `cqp` corpora:

- non-TEITOK/plain CWB entries run through direct `cqp` CLI path.
- TEITOK-style entries (e.g. `supports_xml=true`) run through `python -m flexicorp query --backend cqp --extract-fragments --api` so XML fragment enrichment is preserved.

HTTP mode:

```bash
cargo run -- serve --host 127.0.0.1 --port 8787
```

Local permissive test mode (policy is reported but query still runs):

```bash
cargo run -- serve --host 127.0.0.1 --port 8787 --test
```

By default, `fqs serve` writes a rolling plaintext request log:

```bash
cargo run -- serve --host 127.0.0.1 --port 8787
```

- Default log path:
  - macOS: `/usr/local/var/log/fqs/fqs.log`
  - Linux/Unix: `/var/log/fqs/fqs.log`
  - Windows: `%APPDATA%/fqs/logs/fqs.log`
- If that path is not writable, FQS falls back next to the DB as `fqs-http.log`.
- Rotation controls:
  - `--log-max-bytes` (default `10485760`, i.e. 10MB)
  - `--log-keep-files` (default `7`)
  - optional override: `--log-file /path/to/fqs.log`
- Query calls may include `session_id`; FQS upserts that into SQLite `active_sessions` (`last_seen_at` heartbeat).
- Session cleanup runs at startup and every 60s while serving (`--session-ttl-minutes`, default `120`).

Quick health/status probe (without starting server):

```bash
cargo run -- status --url http://127.0.0.1:8787
```

or:

```bash
cargo run -- status --host 127.0.0.1 --port 8787
```

Endpoints:

- `GET /health` — JSON includes `version`, `db_path`, and optional `server_name` (set via `--server-name` or env `FQS_SERVER_NAME`, e.g. to label a deployment or distinguish host vs container)
- `GET /corpora?request_role=visitor|admin&tag=<browse-label>`
- `GET /labels` — distinct browse labels (for Kontext-style facets)
- `GET /reindex/jobs?status=queued&corpus=<id>&limit=100` — queue/running overview
- `POST /reindex/jobs` — enqueue (`request_role=admin`)
- `GET /reindex/history?corpus=<id>&limit=200` — history/audit log (includes indexed timestamps)
- `POST /reindex/workers/heartbeat` — worker liveness/capacity callback
- `POST /reindex/jobs/mark-started` — worker callback
- `POST /reindex/jobs/mark-finished` — worker callback
- `GET /fcs?operation=explain|searchRetrieve|scan&x-corpus=<id>&x-fcs-context=<id>&query=<cql>`
- `POST /query` with JSON body:
  - `{"corpus":"...","query":"...","language":"auto","start":0,"size":25,"request_role":"visitor","backend":"pando"}`
  - optional **`backend`**: `pando` or `cqp` — TEITOK/flexicorp should set from project config; catalogue `preferred_backend` may be `auto`

In `--test` mode, `/query` adds a `policy` block:
- `would_block`: whether normal locked mode would reject
- `reasons`: policy reasons

FCS notes (base implementation):

- `explain` lists corpora where `capabilities.fcs.enabled=true`
- `searchRetrieve` delegates to existing backend query path and returns basic SRU/FCS XML envelope
- if `operation` is omitted and `query` is present, FQS infers `searchRetrieve` (SRU-friendly default)
- `x-fcs-context` is accepted as an alias for `x-corpus`
- if no context is provided, FQS runs a compatibility search over all FCS-enabled, policy-eligible corpora
- `scan` currently returns a not-implemented diagnostic
- `x-fcs-endpoint-description=true` on `operation=explain` adds `extraResponseData` with `ed:EndpointDescription` and `ed:Resources`

## Quickstart

```bash
cd fqs
cargo run -- init
cargo run -- corpora add --id migrant --label "Migrants Corpus" --project-root /srv/teitok/migrant --preferred-backend pando
cargo run -- corpora show --id migrant
cargo run -- query --corpus migrant --q '[word="the"]'
```

## Commands

Reindex scaffolding:

```bash
cargo run -- reindex enqueue --corpus migrant --backends pando,cqp --priority 10 --origin teitok
cargo run -- reindex queue --status queued --limit 100
cargo run -- reindex history --corpus migrant --limit 200
cargo run -- reindex worker-heartbeat --worker-id worker-a --max-concurrent 2 --capabilities pando,cqp
cargo run -- reindex dispatch-once --default-worker-max-concurrent 1
```

Add corpus:

```bash
cargo run -- corpora add \
  --id tt-eemc \
  --label "EEMC Corpus" \
  --tag "English" --tag "spoken" \
  --project-root /srv/teitok/eemc \
  --project-url https://corpora.example.org/teitok/eemc/ \
  --preferred-backend clickql \
  --environment stable \
  --version-tag 2.1 \
  --family-key ud \
  --family-label "Universal Dependencies" \
  --listing-visibility public
```

Upsert from JSON (inline):

```bash
cargo run -- corpora upsert-json --json '{
  "id":"ud-de-live",
  "label":"UD German Live",
  "project_root":"/srv/teitok/ud_de",
  "preferred_backend":"pando",
  "environment":"live",
  "family_key":"ud",
  "family_label":"Universal Dependencies",
  "version_tag":"live",
  "labels":["UD","German"]
}'
```

Upsert many from file:

```bash
cargo run -- corpora upsert-json --json-file corpora-batch.json
```

Upsert from stdin:

```bash
cat corpora-batch.json | cargo run -- corpora upsert-json --stdin
```

List corpora (default hides superseded `is_current=0`):

```bash
cargo run -- corpora list
```

Filter by browse label (case-insensitive):

```bash
cargo run -- corpora list --tag English
```

List with stable/dev filter and grouped families:

```bash
cargo run -- corpora list --environment stable --group-by-family
```

Show one corpus:

```bash
cargo run -- corpora show --id migrant
```

Mark old version as superseded (hidden from default list):

```bash
cargo run -- corpora supersede --id tt-eemc-v1
```

Remove a catalogue row permanently (destructive):

```bash
cargo run -- corpora delete --id tt-eemc-v1 --force
```

Query (CLI):

```bash
cargo run -- query --corpus migrant --q '[lemma="book"]' --language pando-cql --start 0 --size 25
```

Note: backends other than `pando`/`cqp` currently return `not implemented yet` in `fqs query`.

For pando corpora, prefer `--language pando-cql` (default).
For CWB/CQP corpora, prefer `--language cwb-cql`.

Validate corpus metadata/path checks:

```bash
cargo run -- corpora validate
```

Full validation with backend probes where configured:

```bash
cargo run -- corpora validate --full
```

Strict full mode (mark missing query probe as failure):

```bash
cargo run -- corpora validate --full --strict-full
```

In `--full` mode, `cqp` corpora run a small CQP probe; `pando` corpora run `flexicorp-pando` and persist the JSON `total` as `corpus_size` (override the probe CQL with `settings.pando_probe_query`, default `[word=".*"]`). Pando entries get this probe even when `interfaces` is empty.

## Database

Default DB path follows OS conventions:

- macOS: `/usr/local/var/fqs/fqs.db`
- Linux/Unix: `/var/lib/fqs/fqs.db`
- Windows: `%APPDATA%/fqs/fqs.db`

For Apache/service deployments, ensure the parent directory exists and is writable by the runtime user (e.g. `www-data`, `apache`), or set `FQS_DB_PATH` explicitly.

If you see **`attempt to write a readonly database`** (SQLite error 8), the UID running `fqs` cannot write the `.db` file or the directory that holds it. Typical fixes: create the parent dir and `chown`/`chmod` it for the web server user, or point `FQS_DB_PATH` at a file under your TEITOK project (or `/tmp`) that that user can write—same requirement for `fqs corpora upsert-json` when invoked from PHP.

Override with `--db` on commands, for example:

```bash
cargo run -- corpora list --db /tmp/fqs.db
```

Or set one default once for your shell/server process:

```bash
export FQS_DB_PATH=/var/lib/fqs/fqs.db
```

Schema includes fields needed for catalog concerns from the start:

- `project_root` — TEITOK **project** directory (e.g. `.../infoveillance`), not the `pando` or `cqp` subfolder. If you store `.../infoveillance/pando`, FQS treats the parent as the TEITOK root for CWD / flexicorp; prefer the parent path and rely on default `project_root/pando` for the index, or set `settings.index_dir` explicitly.
- `project_url` (canonical corpus URL; do not infer from filesystem path)
- `http_policy_mode` (e.g. `public_query`, `auth_required`, `disabled`)
- `http_allowed_operations` (per-corpus HTTP operation allowlist, separate from permissive CLI)
- `environment` (free-form deployment label for filtering; e.g. `dev` / `live` / `stable` or site-specific names)
- `is_current` (default listings hide old/superseded corpora)
- `family_key` + `family_label` (for grouped family/subcorpus views)
- `version_tag` (deployment lane, e.g. live/stable)
- `corpus_version` (content / publication version — one catalogue row per corpus + version)
- `interface_preference` (optional; where to browse — TEITOK/Kontext/etc.; informational)
- `preferred_backend`: `pando` \| `cqp` \| **`auto`** (default for `corpora add`; resolves via `settings.query_backend` or index paths)
- `source_kind` + `supports_xml` (non-TEITOK corpora can be registered with reduced capabilities)
- `interfaces` (e.g. `["query","kwic","freq","xml_context"]`)
- `labels` — browse tags for catalog filtering (Kontext-style facets), stored as JSON array
- `capabilities` (JSON object for feature flags/details)
- `settings` (JSON object for corpus-specific runtime settings)
- `created_at` / `updated_at` (first registration, last metadata update)
- `first_corpus_update_at` / `last_corpus_update_at` (content/index update timeline; `first_corpus_update_at` is auto-set on first insert if omitted)
- `corpus_size` + `corpus_size_updated_at` (populated by validation/probe when available)
- `last_validated_at` + `last_validation_ok` + `last_validation_message`

### FCS from day one (metadata convention)

FQS can represent FCS availability for any corpus (TEITOK or non-TEITOK) by storing FCS metadata in `capabilities.fcs`, e.g.:

```json
{
  "fcs": {
    "enabled": true,
    "resource_pid": "local:my_corpus",
    "supports_dataviews": ["hits", "kwic"]
  }
}
```

This keeps cataloging and policy ready for an eventual `/fcs` HTTP surface even before server-side FCS query execution is implemented.

Export current corpus set as JSON:

```bash
cargo run -- corpora export-json
```

Export all (including superseded) to file:

```bash
cargo run -- corpora export-json --include-noncurrent --output /tmp/fqs-corpora-export.json
```

Load mixed TEITOK/non-TEITOK local examples:

```bash
cargo run -- corpora upsert-json --json-file corpora-mixed.local.example.json
```

## Notes

- `fqs query` currently supports `pando` and `cqp`; other backends return `not implemented yet`.
- The HTTP surface is intended as the primary control/query interface for UI integrations.
