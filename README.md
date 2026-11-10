# FQS

FQS is a uniform search server over corpus engines: one HTTP API, a corpus catalogue,
admission limits (query queue), a CLARIN-FCS 2.0 endpoint and an admin UI. Engines are optional
runtime plug-ins: **CWB** (`cqp`), **pando** (via `libflexicorp_pando`, or pando's own CLI) and,
through FCS, **Manatee** (via a KonText JSON API). A CWB-only installation needs nothing but `cqp`.

It works on its own, e.g. as an FCS endpoint for a CLARIN centre's CWB corpora. TEITOK and
KonText integrate with it (as *frontends*), but neither is required. Only two optional features
use the TEITOK integration package **flexicorp** (Python): reindexing TEITOK projects, and XML
fragments for TEITOK corpora served by CWB (`supports_xml`). See `PYTHON_BIN` below.

## Installation (systemd)

Building needs **Rust 1.85 or newer** (edition 2024). Distribution packages are often older (Ubuntu 22.04 ships cargo 1.75), so use [rustup](https://rustup.rs) as a normal user (`curl https://sh.rustup.rs -sSf | sh`, then `source ~/.cargo/env`). Build as that user, then install with sudo and `--skip-build`. Build on the target system (or for `x86_64-unknown-linux-musl`): a binary built on a newer distribution may not run on an older glibc.

On a Linux server, install the binary, catalog dirs, admin UI, and a `fqs.service` unit:

```bash
# optional: cargo build --release && pass --skip-build
sudo ./install.sh --web-user www-data
# with TEITOK (flexicorp installed in the TEITOK shared venv), additionally:
#   --teitok-venv /var/www/html/teitok/shared/Resources/venv --scan-root /var/www/html/teitok
# with pando's warm library (built in flexicorp_pando), additionally:
#   --pando-lib /path/to/flexicorp_pando/build/libflexicorp_pando.so
```

What it does:

| Piece | Path |
|---|---|
| Binary | `/usr/local/bin/fqs` |
| Pando library (`--pando-lib`) | `/usr/local/lib/fqs/libflexicorp_pando.so`, `FLEXICORP_PANDO_LIB` in `fqs.env` |
| System config | `/etc/fqs/fqs.json` (`db_path`, `scan_roots`, admin restart via systemctl) |
| Secrets / env | `/etc/fqs/fqs.env` (`FQS_SECRET`, **`PYTHON_BIN`**, `FQS_SERVER_NAME`, …) |
| Catalog + WAL | `/var/lib/fqs/` (mode `2775`, user `fqs`, group `fqs`) |
| Logs | `/var/log/fqs/` |
| Admin static UI | `/usr/local/share/fqs/admin` → `http://127.0.0.1:8790/admin/` |
| Unit | `/etc/systemd/system/fqs.service` (`systemctl enable --now fqs`) |

The installer creates system user/group `fqs` and adds `--web-user` (default `www-data`) to group `fqs` so TEITOK PHP and `fqs serve` can both write the SQLite catalog. **Restart php-fpm** after install so the new group membership applies.

`PYTHON_BIN` is **only** needed for the two TEITOK features (reindexing TEITOK projects; CWB corpora registered with `supports_xml`). It must then point at a Python that can `import flexicorp` (normally the TEITOK shared venv); without it those features fail with `No module named flexicorp`. Plain CWB, pando and FCS never use it. Flexicorp itself is installed separately into that venv:

```bash
/var/www/html/teitok/shared/Resources/venv/bin/python -m pip install -e /path/to/flexicorp
```

**Pando library.** pando corpora run fastest through `libflexicorp_pando`, loaded into FQS
(warm). FQS uses `FLEXICORP_PANDO_LIB` when set (only that file); otherwise it looks next to the
`fqs` binary, then in `/usr/local/lib` and `/usr/lib` (a development build also looks in
`../flexencoder/flexicorp_pando/build` beside its checkout). A library that is found but cannot
be used (an old build missing a symbol, a wrong file) is reported and the next place tried.
Without a library, pando queries run through the cold `flexicorp-pando` CLI
(`FLEXICORP_PANDO_BIN` or `PATH`; pando-cql only), and fail when that is missing too. FQS prints
what it checked at startup; `/health` shows `pando.mode` (`library`, `cli` or `unavailable`) and
the admin health / backends view the library path and the error. Install the library with each
pando update (an old copy in `/usr/local/lib` is the usual cause of `cli` mode) and restart.

Templates live under `deploy/` (`fqs.service`, `fqs.env.example`). After changing env:

```bash
sudo systemctl restart fqs
sudo systemctl status fqs
curl -sS http://127.0.0.1:8787/health
```

Corpus trees that reindex writes into (e.g. `…/migrantstories/pando`) must be writable by user `fqs` or group `fqs`. The unit’s `ReadWritePaths=` includes `/var/www/html/teitok` when present; **every** path listed must exist or systemd fails with `status=226/NAMESPACE`. Add further roots with a drop-in (install.sh does this for configured scan roots):

```bash
sudo systemctl edit fqs
# [Service]
# ReadWritePaths=/data/corpora
```

Useful flags: `--skip-build`, `--no-systemd`, `--no-start`, `--user` / `--group` to override the service account.

Current capabilities:

- native CLI executable (`fqs`)
- SQLite-backed corpus catalog (`fqs.db` by default)
- `corpora add` / `corpora list` / `corpora show` commands
- `query` command with live execution for `pando` corpora (via `flexicorp-pando` CLI)
- `corpora validate` command (basic + full probe mode; optional `--enrich`)
- `corpora enrich` — one-shot languages/features/interfaces from on-disk TEITOK/pando/cqp layout
- `corpora import` — catalogue rows from existing TEITOK and KonText installations, for `upsert-json` (see below)
- reindex control-plane and execution lifecycle:
  - `fqs reindex enqueue|queue|history|mark-started|mark-finished`
  - `fqs reindex dispatch-once` and `fqs reindex worker-heartbeat`
  - SQLite tables `reindex_jobs` + `reindex_history`
  - SQLite table `reindex_workers` (heartbeat/capacity)
  - HTTP routes `GET/POST /reindex/jobs`, `GET /reindex/history`,
    `POST /reindex/workers/heartbeat`, `POST /reindex/jobs/mark-started`,
    `POST /reindex/jobs/mark-finished`
  - every `fqs serve` runs a reindex worker for its own queue (jobs run on its machine and write
    into the corpus folders); `--no-reindex-worker` (or `FQS_NO_REINDEX_WORKER=1`) turns it off
    and makes `POST /reindex/jobs` answer 409: for a query server over read-only data, with the
    jobs going to an FQS that can write the corpora

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

### Admin UI (v0)

Thin catalog / health admin surface (growable; see `dev/FQS-ADMIN-GUI.md`).

```bash
export FQS_SECRET='your-shared-hs256-secret'
# Prefer a loopback admin bind in production (LINDAT):
cargo run -- serve --host 0.0.0.0 --port 8787 \
  --enable-admin-http --admin-bind 127.0.0.1:8790 \
  --activity-log /var/log/fqs/activity.jsonl
# Mint a short-lived admin JWT (aud=fqs-admin, max 4h):
cargo run -- admin-token --user ops --ttl 4h
# open http://127.0.0.1:8790/admin/  (or proxy only that bind)
```

**Through TEITOK (no token to paste):** `index.php?action=fqsadmin` (TEITOK module
`teitok_teitok_ui/fqsadmin.php`) serves this UI from the admin listener and forwards
its API calls, signing each one server-side with a 2-minute admin token
(`aud=fqs-admin`, `user` = the TEITOK user, so the audit log shows who). Only TEITOK
admins listed in `flexicorp/fqs_admin_users` (comma-separated; `*` = every admin of
that project; unset = off) get in; API calls must carry the page's CSRF value
(`X-FQS-Admin-CSRF`) and come from the same origin. Settings: `flexicorp/fqs_admin_url`
(default `http://127.0.0.1:8790`, the `--admin-bind`) and `flexicorp/fqs_secret`.
The admin listener can stay on localhost; no proxy rule or token handling needed.

- **Off by default.** `--enable-admin-http` refuses to start without `--jwt-secret` / `FQS_SECRET`.
- **Separate bind:** `--admin-bind host:port` / `FQS_ADMIN_BIND` serves `/admin` only there (recommended). Without it, admin shares the public port (dev convenience; warns at startup).
- **UI:** static files in `fqs/admin/` at `/admin/` (`--admin-dir` / `FQS_ADMIN_DIR`). CSP + `frame-ancestors 'none'`. Token kept in **sessionStorage** only.
- **Admin JWT** (not a TEITOK query token): HS256 with `role=admin`, **`aud=fqs-admin`**, required **`iat`/`exp`**, TTL ≤ **4 hours**. Mint with `fqs admin-token`.
- **API** (all require that admin JWT; errors are `application/json`):
  - `GET/PUT /admin/api/corpora` — list (incl. superseded) / upsert
  - `GET/DELETE /admin/api/corpora/{id}` — get; **DELETE only deactivates** (`?supersede=1` / `is_current=0`). Hard remove is CLI-only: `fqs corpora delete --id … --force`
  - `POST /admin/api/corpora/{id}/validate` — body `{ "full": false, "strict_full": false }`
  - `GET/POST /admin/api/scan` — discovery; request `roots` are **intersected** with `FQS_SCAN_ROOTS` / `fqs.json` `scan_roots` (else catalog/defaults); cannot walk arbitrary paths
  - `GET /admin/api/self` — this FQS version + update check (default: `main` `fqs/Cargo.toml` on GitHub; override with `fqs.update_check_url` / `FQS_UPDATE_CHECK_URL`)
  - `POST /admin/api/self/restart` — only when `fqs.restart` is set in `/etc/fqs/fqs.json` (same methods as frontend restart)
  - `GET /admin/api/health` — full diagnostics (db path, slots, pando, limits)
  - `GET /admin/api/settings` — **report-only** effective process settings (bind, db, auth trust, limits file, warm pool, FCS, logs, scan allowlist) with CLI/`fqs.json` how-to-change hints; no secret values; not editable via API
  - `GET /admin/api/frontends` — frontend kinds / instances; `GET /admin/api/coverage` — KonText corplist gaps + FCS not-yet-enabled
  - `POST /admin/api/frontends/{id}/publish` — body `{ "corpus_id": "…", "name": "…" }`: publish a catalogue corpus to a frontend through its frontend module (see below); reports `steps`, `complete`, `restart_needed`, and merges the module's `catalog_settings` into the corpus (KonText: `settings.kontext.corpname` / `public_url`). `…/corplist/append` is the older name of the same call
  - `POST /admin/api/corpora/{id}/fcs-enabled` — body `{ "enabled": true|false }` for FCS Add / Exclude
  - `GET /admin/api/activity` — activity-log overview when `--activity-log` / `FQS_ACTIVITY_LOG` is set (`?event=interesting|query|warm|admin|all&limit=&corpus=`); summary + recent events from a tailed window
- **Behind a path-stripping proxy** (e.g. hub `/services/test-kontext/fqsadmin/` → `/fqsadmin/`): set `FQS_ADMIN_BASE_HREF=/services/test-kontext/fqsadmin/` so `index.html` gets a `<base href>` and CSS/JS/API resolve under that prefix. Prefer a trailing-slash public URL.
- **Audit:** each write (upsert, supersede/deactivate, validate, scan, restart) appends an activity-log line when `--activity-log` is set (`by`, corpus/frontend id, before/after hash where applicable).
- **Public `GET /health`** is minimal (`ok`, `service`, `version`, `server_name` only). Catalog `db_path` is on admin health and in the `fqs-http.json` sidecar (TEITOK should prefer the sidecar when public health has no `db_path`).
- **UI tabs:** Corpora, Scan, Backends, Frontends, Settings (report-only), Activity, Health, Reindex jobs.
- **Scan allowlist:** `FQS_SCAN_ROOTS` / `fqs.json` `scan_roots` (see `fqs.example.json`). Binary defaults are only generic paths (`/srv/teitok`, `/data/corpora`, `~/corpora`, …) — not developer trees.

### Frontend modules

What FQS does *for* a frontend lives in a frontend module (`src/frontends/`), not in
core FQS: finding the corpora the frontend lacks (the Frontends tab), and publishing
a catalogue corpus to it. A module implements the `FrontendModule` trait in
`src/frontends/mod.rs` (kind, discovery on this machine, processes, coverage, publish)
and is listed in `modules()`; core FQS and the admin UI only use that interface, so a
module for Korp, CQPweb or NoSketch Engine can be added next to `kontext.rs`. Modules
share the safe file writing (through symlinks to the real file, a `.fqs-bak-<time>`
backup, mode and owner kept, atomic when possible) and the Pando index helpers.

**KonText (`kontext.rs`).** The KonText card shows KonText's processes (Sanic or
gunicorn) and, per corpus, what KonText still lacks; **Add to KonText** does all of it.
It only offers corpora that already have a Pando index: those are the ones
kontext-pando serves through FQS. Manatee corpora are KonText's own (they do not go
through FQS), and a corpus without an index has to be built first — publishing only
lists corpora that are ready.
A corpus opens in KonText (kontext-pando) when:

1. it is in `corplist.xml` — FQS adds it (checked to be well-formed XML);
2. `pando_corpora.json` sends its queries to Pando — FQS adds `"backend": "fqs"` with
   this FQS's URL (no `size`: kontext-pando asks FQS's `/info`, which stays right after
   a reindex);
3. Manatee has a registry file for it, since KonText opens every corpus with
   `manatee.Corpus(registry)` and refuses one whose `PATH` folder does not exist — FQS
   writes the registry from the Pando index's `corpus.info` (positional attributes,
   structures and their attributes, multivalue attributes, language, DOCSTRUCTURE /
   FULLREF) and creates its `PATH` folder. kontext-pando answers concordances,
   frequencies, text types and the corpus info from Pando, so the Manatee data is not
   read for those. Only when Manatee's `encodevert` is available does FQS also encode a
   one-token vertical (a "shell"), so that the KonText functions that still read Manatee
   data directly (word list, keywords, collocations) find a tiny corpus instead of
   missing files; they do not give real results for Pando corpora either way. The
   registry carries a `# fqs: corpus=… index_id=…` line: after a reindex the card shows
   it as outdated and **Add to KonText** rebuilds it (and encodes it once `encodevert`
   is found); registries FQS did not write are never touched;
4. KonText is restarted (offered when it can be: a `restart` block in fqs.json, or a
   restart trigger, below) and users have access to the corpus in KonText's auth — that
   last step is KonText's own.

FQS must be able to write these files: the systemd unit has `ProtectSystem=strict`, so
their folders need `ReadWritePaths` (a drop-in), and the service user needs write
permission. `fqs frontends paths` lists them; `install/install-stack.pl` sets both up
(an ACL for the service user, `/etc/systemd/system/fqs.service.d/frontends.conf`). The
KonText card says which of the two is missing for each file.

FQS edits only files at paths from fqs.json, the environment (`FQS_KONTEXT_CORPLIST`,
`PANDO_CORPORA_CONFIG`, `MANATEE_REGISTRY`) or KonText's own `config.xml` / install
folder — never from the request — and needs write access to them (and to the Manatee
registry, data and vert folders). Registry data and verticals go next to the registry
folder (`/var/lib/manatee/{registry,data,vert}`) unless `manatee_data` / `manatee_vert`
say otherwise. `public_url` is what corpus lists link to; `fqs_url` is where KonText
reaches FQS (default: FQS's own address):

```json
{
  "frontends": [
    {
      "id": "kontext",
      "kind": "kontext",
      "label": "KonText",
      "url": "http://127.0.0.1:8080",
      "public_url": "https://example.org/kontext",
      "corplist": "/opt/kontext/conf/corplist.xml",
      "pando_corpora": "/opt/kontext/conf/pando_corpora.json",
      "registry": "/var/lib/manatee/registry",
      "manatee_data": "/var/lib/manatee/data",
      "manatee_vert": "/var/lib/manatee/vert",
      "encodevert": "/usr/bin/encodevert",
      "fqs_url": "http://127.0.0.1:8787",
      "restart": { "method": "systemctl", "unit": "kontext" }
    }
  ]
}
```

Current KonText runs under Sanic (`kontext.service` on test-kontext); older ones under gunicorn (`"unit": "gunicorn"`). Other restart methods: `hup_pidfile` with `"pidfile": "/run/gunicorn.pid"`, or `argv` with an allowlisted command array.

**Restart triggers.** FQS runs as an unprivileged user with `NoNewPrivileges`, so a
`systemctl restart` it runs itself is refused. `install/install-stack.pl` therefore sets up,
as root, one systemd path unit per unit that may be restarted: `fqs-restart-<unit>.path`
watches `/var/lib/fqs/restart/<unit>` (`FQS_RESTART_DIR` overrides the folder) and starts
`fqs-restart@<unit>.service`, which runs `systemctl restart <unit>.service`. A `systemctl`
restart (from fqs.json, or `fqs.restart`) for a unit with such a file writes the file instead and
waits until the unit is active again (30 s). A KonText that is not in fqs.json (found on this
machine or named by the catalogue) is restartable when `/var/lib/fqs/restart/kontext` exists,
and FQS itself when `/var/lib/fqs/restart/fqs` exists. By hand:

```bash
sudo install -d -m 0755 /var/lib/fqs/restart
sudo install -m 0644 -o fqs /dev/null /var/lib/fqs/restart/kontext   # the FQS service user
# /etc/systemd/system/fqs-restart@.service:  [Service] Type=oneshot / ExecStart=/bin/systemctl restart %i.service
# /etc/systemd/system/fqs-restart-kontext.path: [Path] PathModified=/var/lib/fqs/restart/kontext / Unit=fqs-restart@kontext.service
sudo systemctl daemon-reload && sudo systemctl enable --now fqs-restart-kontext.path
```

`fqs corpora upsert-json` on an existing corpus keeps the fields the JSON leaves out,
and the choices made in the admin (`settings.fcs.enabled`, `settings.kontext`
`corpname` / `public_url` / `url`) unless the JSON sets them, so re-registering a
TEITOK project does not undo them; `--replace` replaces the row as given.

Example FQS self-restart / update check in `/etc/fqs/fqs.json`:

```json
{
  "fqs": {
    "update_check_url": "https://raw.githubusercontent.com/ufal/fqs/main/Cargo.toml",
    "restart": { "method": "systemctl", "unit": "fqs" }
  }
}
```

Set `"update_check": false` under `fqs` to skip the remote version probe.

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
  - `--log-max-bytes` (default `104857600`, i.e. 100 MB)
  - `--log-keep-files` (default `10`)
  - rotated files are gzipped (`fqs.log.1.gz` … `fqs.log.N.gz`, also for the activity log); `--no-log-compress` keeps them plain. Read them with `zcat` / `zgrep`.
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

- `GET /health` — minimal public liveness (`ok`, `service`, `version`, optional `server_name`), and per engine whether it can run and how: `engines: {"pando": {"available": true, "mode": "library"}, "cqp": {…, "mode": "process"}, "manatee": {…, "mode": "native"}}`. Full details on `GET /admin/api/health` (admin JWT).
- `GET /backends` — one entry per engine in the same shape: `available`, `mode`, `version`, `error` when it can't run, the number of current `corpora` it serves, and for native Manatee `workers`, `workers_running` (the others start on first use) and `workers_busy`. Monitoring can alert on `available: false` (e.g. Manatee corpora in the catalogue, Manatee not available). File paths and detailed errors (which may name paths) only for an admin caller (admin JWT); `pando` keeps the library's status, without paths for others. Native Manatee here is FQS's own (`FQS_MANATEE_LIB`), not KonText's Python bindings, which `/admin/api/backends` lists apart.
- `GET /corpora?request_role=visitor|admin&tag=<browse-label>&frontend=teitok&facet=lang:cs&facet=feature:spoken&q=…&view=browse` — catalog list; `frontend=teitok` keeps TEITOK-listable rows; repeated `facet=` is AND across groups / OR within; `view=browse` returns a public DTO (no `project_root` / settings dump)
- `GET /labels?frontend=teitok&facet=…` — distinct browse labels plus `facets` groups with counts (KonText-style)
- `GET /reindex/jobs?status=queued&corpus=<id>&limit=100` — queue/running overview
- `POST /reindex/jobs` — enqueue (`request_role=admin`)
- `GET /reindex/history?corpus=<id>&limit=200` — history/audit log (includes indexed timestamps)
- `POST /reindex/workers/heartbeat` — worker liveness/capacity callback
- `POST /reindex/jobs/mark-started` — worker callback
- `POST /reindex/jobs/mark-finished` — worker callback
- `GET /fcs` — CLARIN-FCS 2.0 endpoint (SRU 1.2 + 2.0): `explain`, `searchRetrieve` with `query`, `queryType=cql|fcs`, `x-fcs-context`, `x-fcs-dataviews=adv`, `startRecord`, `maximumRecords` (see *FCS endpoint* below)
- `POST /query` with JSON body:
  - `{"corpus":"...","query":"...","language":"auto","start":0,"size":25,"request_role":"visitor","backend":"pando"}`
  - optional **`backend`**: `pando` or `cqp` — TEITOK/flexicorp should set from project config; catalogue `preferred_backend` may be `auto`
  - **`language`**: the query's language, one the corpus accepts with that engine (`src/query_lang.rs`):
    pando `pando-cql` (default), `cwb-cql`, `manatee-cql`, and `pmltq` / `tiger` for a corpus labelled
    for them (pando translates them; it needs FQS's pando library); CWB `cwb-cql`; Manatee
    `manatee-cql`. **The labels decide, not the index:** `pmltq` needs the label `feature:dependencies`
    (any scheme: UD, PROIEL, …; `feature:ud` alone is UD morphology and does not), `tiger` `feature:constituency` (or `feature:trees`), in the corpus's `labels` or
    `capabilities.browse.features`. FQS does not look into the index per query: `fqs corpora enrich`
    sets `feature:dependencies` when `deprel` and `head` are indexed; `feature:constituency` is set by
    hand. A UD corpus without the label is offered pando CQL, CWB CQL and Manatee CQL only.
    Aliases: `cqp`, `pml-tq`, `tigersearch`, `native`. Another language: 400 with the list the
    corpus accepts. A corpus can narrow its list with `settings.query_languages`. The same
    `language` on `POST /run` (programs). `GET /corpora` gives each corpus's `query_languages` (the
    default first), so a front end offers exactly those (e.g. a query-language choice next to KonText's
    "advanced" query). A query can also name its language as its first word, in capitals: `PMLTQ`,
    `PML-TQ`, `CWB`, `CQP`, `TIGER`, `PANDO`, `MANATEE` (`PMLTQ a-node [ deprel = "nsubj" ]`). The word
    is taken off and replaces `language`, so a front end without a language choice (KonText's CQL field)
    can send PML-TQ or CWB CQL as it is. On `/run` the rest of the program can follow in pando CQL
    (`PMLTQ a-node [ lemma = "be" ]; freq by upos`).

  - **`aligned`** (native Manatee, a parallel corpus: one corpus per language, each naming the others
    in its registry's `ALIGNED`): `true` (all of them) or registry names (`"czeng_en"`, `["a","b"]`).
    The answer gets `aligned_corpora` and per hit `aligned: [{corpus, pos, end, text, tokens, match?}]`,
    the aligned segment of each (as KonText's aligned column; `pos` null when the hit has none there).
    Filtering by the other language is Manatee CQL as KonText writes it: `[lemma="pes"] within
    czeng_en:[lemma="dog"]` (`within!` for "not"); `match` is then the part of the segment it matched.
    The aligned corpora's registry files must be in the same folder; `fqs corpora import` lists them in
    `settings.manatee_aligned`. Needs libfqs_manatee 0.5 (`manatee/build.sh`); test data and a check
    against Manatee's Python module in `testdata/manatee-parallel/`.

  - **`kwic_view`** (native Manatee, pando): KonText's KWIC lines, `{left, right, attrs, ctxattrs,
    structs, refs}` as KonText gives them to Manatee's `KWICLines` (`"-40#"`, `"40#"`, `"word,lemma"`,
    …; `"-1:s"` for sentences; pando also `maxctx`, the registry's MAXCONTEXT). Each hit gets
    `kwic_view: {pos, kwiclen, ctxbeg, ctxend, linegroup, refs, left, kwic, right}`: the line as
    `KWICLines` gives it (strings and classes alternating), so KonText builds its page with its own
    code. Native Manatee runs `KWICLines` itself (libfqs_manatee 0.8); pando has the same algorithm
    (`kwic_view` in pando's `/query` and `/run`). Check against Manatee's Python module:
    `testdata/manatee-kwic-view/check-kwic-view.py`. The attribute delimiter is the Manatee build's
    (`/` upstream, U+001F in ÚČNK's manatee-open, which KonText runs and pando follows).

  - **`default_attr`** (native Manatee, pando): the attribute a bare `"string"` stands for (KonText's
    simple query with a chosen attribute, `a<attr>,<query>`); the query is not changed. Native Manatee
    sets the corpus's default attribute for that query (and restores it), pando's Manatee and CWB
    dialects read bare strings with it. Totals and background counts keep queries with another default
    attribute apart.

  - **`arf`** (native Manatee): `true` adds Manatee's ARF of the result to the counted total
    (`result.arf`, `page.arf`; with `total: "async"` the `/status` of the count). For pando, `POST /run`
    with `…; arf`.

  - **`subcorpus`** (pando, native Manatee, CWB): the name of a stored subcorpus (see *Stored subcorpora*);
    `subcorpus_mode` `use` (default) or `within`, `subcorpus_owner` when several of that name are
    visible. The answer gets `subcorpus: {name, mode, tokens, owner}`. The same fields on `POST /run`
    and as query parameters on `GET /info`.

In `--test` mode, `/query` adds a `policy` block:
- `would_block`: whether normal locked mode would reject
- `reasons`: policy reasons

### FCS endpoint

`/fcs` is a CLARIN-FCS Core 2.0 endpoint (SRU 2.0, and SRU 1.2 / FCS 1.0 for
older clients). The code is `src/fcs/` and does not depend on the rest of FQS
or on flexicorp: FQS hands it the resources (catalogue rows with
`fcs.enabled`), an engine per resource, and does access control and admission
around it.

- **explain**: ZeeRex record; with `x-fcs-endpoint-description=true` the
  Endpoint Description (version 2 for SRU 2.0, 1 for 1.2): Basic + Advanced
  Search, Hits + Advanced views, layers `word lemma pos …`, one resource per
  corpus (PID, titles, descriptions, institution, landing page, ISO 639-3
  languages).
- **searchRetrieve**: Basic Search (CQL: terms, `"phrases"`, `*` / `?` masking,
  `AND` = same sentence, `OR`, parentheses) and Advanced Search (FCS-QL: segments
  with `& | !`, `=` / `!=`, regex flags `/c /l /d`, `[]`, quantifiers, `|`,
  groups, `within s|p|text|…`). One record per hit with the Hits view, plus the
  Advanced view (text, lemma, pos …; match highlighted) when `x-fcs-dataviews=adv`.
  `x-fcs-context` takes PIDs or corpus ids; without it all resources are searched
  one after the other and records are numbered across them.
- **Diagnostics** instead of engine errors: CQL syntax → SRU 10, unsupported CQL
  → SRU 48, FCS-QL syntax → FCS 5, unsupported FCS-QL (layer / scope the corpus
  does not have, a construction the engine cannot run) → FCS 6, bad PID → FCS 1,
  view not available → FCS 4, busy → SRU 2. `scan` → SRU 4.
- **Engines** (the query is translated per engine; `settings.fcs.engine` or the
  corpus's backend picks one):
  - *pando*: `settings.pando_server: "http://host:port"` (any `pando-server`),
    else the warm library (libflexicorp_pando, see Installation), else pando's own command line
    (`settings.pando_binary`, `PANDO_BINARY` or `pando` on the PATH; one process
    per request, under the process slots). pando has no `|`
    between sequences and no quantified groups: those are expanded into several
    queries whose union is the result (at most 32).
  - *CWB*: `cqp` run directly (`size` + `tabulate`), with `settings.registry_hint`
    (or `CWB_REGISTRY`), `corpus_name` / `cqp_corpus`, `cqp_binary`. TEITOK CWB
    corpora work the same way (no flexicorp CLI).
  - *Manatee*: through KonText's JSON concordance (`view?…&format=json`, old and
    new KonText): `settings.kontext: {"url": "https://…/kontext", "corpname": "x"}`
    (or the `base` / `corpus` of a `kontext` hit link). No Python in FQS.
- Records' `ref`: the landing page for the resource, `hit_link` per hit
  (presets `kontext`, `kontext_first`, `teitok`, `cqpweb`, `korp`, or a
  template with `{n} {pos} {end} {doc} {tokid} {cql} {id} {pid}`). `kontext`
  (KonText ≥ 0.16) links each hit to its own concordance line,
  `{base}/create_view?corpname={corpus}&q=q<query>&pagesize=1&fromp={n}`, and
  gives the resource `{base}/query?corpname={corpus}` as landing page unless
  `landing_page` is set. A Manatee resource without a `hit_link` gets the same
  from `settings.kontext` (`public_url`, else `url`).
- `--fcs-base-url` / `FQS_FCS_BASE_URL`: the public URL of `/fcs` (default PIDs
  `<base>/resource/<id>`, layer ids `<base>/layers/<layer>`).
- FCS requests are anonymous (visitor, or the role of a signed token); `cqp`
  takes a process slot; the activity log records them with `query_type`,
  `resources`, the `native` queries and `total`.
- Test: `tests/fcs_same_data.py <fcs-url> <resource>…` sends a set of Basic and
  Advanced queries to resources that hold the same corpus and flags differing
  totals or first pages.

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

Rows for corpora that existing installations already have, instead of registering them one by one:
`corpora import` reads a TEITOK folder (every project's `settings.xml` and indexes, the shared
corpus list) and/or KonText's `corplist.xml` with the Manatee registry files (the corpus's name,
data path, description, language, and its attributes with their labels, as `/info` gives them),
and writes rows for `upsert-json`, a review table and a summary. It writes nothing to the
catalogue: check the table, then load the file. Run it on each old server with what that server
has (`--map-path OLD=NEW` gives the paths as the FQS server will see them); `--existing` (an
`export-json`) keeps what is set in the catalogue already, filling only empty fields. KonText's
access groups are kept in `settings.access`: FQS does not check them, so the corpora that are not
open to anonymous users are listed. `fqs corpora import --help` has all options.

```bash
fqs corpora import --teitok /var/www/html/teitok --teitok-url https://example.org/teitok \
    --kontext-corplist auto --kontext-url https://example.org/kontext \
    --out rows.json --report rows.tsv
fqs corpora upsert-json --json-file rows.json
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

One-shot catalogue enrich (languages; spoken/timealigned/facsimile/video/geo/dialect/deps/parallel/ner/ud features; interfaces — additive; see the TEITOK corpora-listing design notes in the flexicorp repository):

```bash
cargo run -- corpora enrich --dry-run
cargo run -- corpora enrich --id my_corpus
cargo run -- corpora enrich --id my_corpus --reset-features   # feature labels = what is detected now
cargo run -- corpora validate --full --strict-full --enrich --id my_corpus
```

`--reset-features` replaces the `feature:` labels instead of only adding to them (the report lists `removed_labels`); other labels stay.

Audio (`spoken`, `timealigned`), video and facsimile come only from real evidence: a non-empty `Audio/`, `Media/`, `Video/` or `Facsimile/` folder, or a sample of the documents (up to 30 files under `xmlfiles/`): `<media>` with an audio or video type or extension, `start=` / `begin=` times or `<timeline>`, and `facs=`, `<facsimile>`, `<surface>` or `bbox=`. Words in `settings.xml` do not count, because stock settings (the teiHeader template, menus) mention audio, facsimiles and geolocation in projects that have none. Geolocation needs a `<geomap>`, coordinate or country fields (`key="lat"`, …), `Resources/geo.json` or a `Geo/` folder.

Validation records `corpus_size`: the `size=` of the Pando index's `corpus.info` (also for a quick validation), else, in `--full` mode, the total of a small probe (`cqp` corpora: a CQP probe; `pando`: `flexicorp-pando` with `settings.pando_probe_query`, default `[word=".*"]`). Pando entries get this probe even when `interfaces` is empty. Saving an entry without a size (registration, scan, admin form) and a reindex take it from `corpus.info` too.

### Manatee corpus → pando corpus

`fqs manatee-export --registry FILE --out FILE.jsonl` writes a compiled Manatee corpus as pando's
JSONL v2 (positional attributes with their names, structures with their attributes, multivalue,
zero-width and nested ranges, UTF-8), for a pando corpus when the vertical is gone:

```bash
fqs manatee-export --registry /path/to/registry/corpus --out corpus.jsonl
pando-index --format jsonl corpus.jsonl /path/to/pando/corpus
```

It needs libfqs_manatee 0.6 (`--lib`, else `FQS_MANATEE_LIB`, else the one in `/etc/fqs/fqs.env`),
checks the corpus first as before a query, and prints a summary (tokens, regions per structure,
attributes left out). `--limit N` writes the first N positions only; `--dynamic` includes dynamic
attributes (by default left out: pando has `%c`/`%d`). `testdata/manatee-export/compare.py` counts the
same queries on the Manatee corpus (through FQS) and on pando corpora made from it.
Details and the checks so far: `dev/MANATEE-NATIVE.md`, "Export to pando".

## Database

Which catalog `fqs` uses (every command, `serve` included) is the first of:

1. `--db <file>`
2. `FQS_DB_PATH`
3. `"db_path"` in the system config file `/etc/fqs/fqs.json` (or the file named by `FQS_CONFIG`)
4. the catalog of a running `fqs serve` (its `fqs-http.json` in `/var/lib/fqs` or `/usr/local/var/fqs`)
5. the default: `/var/lib/fqs/fqs.db` on Linux, `/usr/local/var/fqs/fqs.db` on macOS, `%APPDATA%/fqs/fqs.db` on Windows

For a server, write the config file once, so the service, an admin shell and TEITOK's PHP
(`fqs corpora upsert-json` as `www-data`) all use the same catalog, whatever their environment:

```bash
sudo mkdir -p /etc/fqs /var/lib/fqs
echo '{"db_path": "/var/lib/fqs/fqs.db"}' | sudo tee /etc/fqs/fqs.json
```

`fqs status` prints `db_path` and `db_source` (which rule chose it). Public `GET /health`
is minimal (no `db_path`); use admin `GET /admin/api/health` or the `fqs-http.json` sidecar
written beside the catalog when `fqs serve` starts. `fqs serve` also logs the catalog path at start.
When the chosen file does not exist yet, every
command says so on stderr (it then creates a new, empty catalog): the usual sign that it is not the
catalog you meant.

For Apache/service deployments, ensure the parent directory exists and is writable by the runtime user (e.g. `www-data`, `apache`).

If you see **`attempt to write a readonly database`** (SQLite error 8), the UID running `fqs` cannot write the `.db` file or the directory that holds it. Typical fixes: create the parent dir and `chown`/`chmod` it for the web server user, or point `FQS_DB_PATH` at a file under your TEITOK project (or `/tmp`) that that user can write—same requirement for `fqs corpora upsert-json` when invoked from PHP.

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

### FCS metadata (`settings.fcs`)

A corpus is an FCS resource when `settings.fcs.enabled` (or the older
`capabilities.fcs.enabled`) is true; `settings.fcs` overrides `capabilities.fcs`
member by member. Schema: `dev/FQS-FCS2-IMPLEMENTATION.md` §4, e.g.:

```json
"fcs": {
  "enabled": true,
  "pid": "http://hdl.handle.net/11234/1-5287",
  "title": {"en": "UD 2.18"}, "description": {"en": "…"}, "institution": {"en": "ÚFAL"},
  "landing_page": "https://…", "languages": ["en"],
  "layers": {"text": "form", "lemma": "lemma", "pos": {"attr": "upos"}},
  "dataviews": ["hits", "adv"],
  "sentence": "s", "within": {"text": "doc"},
  "hit_link": {"frontend": "kontext", "base": "https://…/kontext", "corpus": "ud_pando"},
  "engine": "pando | cwb | kontext", "context": 5, "doc_attr": "text_id", "tokid_attr": "id"
}
```

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

## Named queries: reserved name `LAST`

Flexicorp UIs (Frequency, Advanced stats, etc.) should work with **named queries** as a single abstraction: toggles, scope chips, and contrast sets refer to **names**, not ad‑hoc “last search” APIs.

**Do not treat “last query” as a separate code path.** Backends already expose the full set of named queries via their **show named**–style listing (in ClickCQL this surfaces as the `show_named_query` statement type in the AST pipeline). **`LAST`** is simply one row in that list—*the most recently executed query in the active session / corpus context*—not something the UI should fetch from session storage by itself when a named-query list is available.

Corpus WorkBench (CQP) and Pando already include `LAST` in that listing; **FQS should normalize the same behavior** for other backends so every client consumes **one** named-query enumeration and never special-cases “last” beyond recognizing the reserved name `LAST` when it appears.

Renaming or friendly labels belong in UI/config (e.g. flexicorp.php / query library): they are **presentation**, not a second parallel notion of “last”. The canonical slot stays `LAST` unless an explicit alias layer is introduced.

## Roles, tiers and limits

FQS decides **who** is asking and **how much** they may use; the engine (pando,
through `libflexicorp_pando` api ≥ 4, see Installation) enforces what one request may do.

**Roles.** Callers send `request_role`: `visitor` (not logged in), `user`
(logged in), `admin` (corpus / server admin; `corpus_admin`, `server_admin`
are accepted too). With a shared secret (`--jwt-secret`, env `FQS_SECRET` —
the one TEITOK already signs its `Authorization: Bearer` HS256 tokens with) the
role comes **only** from a valid, unexpired token's `role` claim, and a request
without one is a visitor; `/health` → `limits.role_trust: "jwt"`. Without a
secret the `request_role` field is believed (`"unverified"`) — only acceptable
when FQS is reachable from the front-ends alone.

**Tiers** (`--limits FILE`, env `FQS_LIMITS`): one object per role.

```json
{"heavy_slots": 8, "default_tier": "visitor",
 "tiers": {
   "visitor": {"slots": 2, "per_user": 1, "queue_ms": 3000,
               "timeout_ms": 20000, "total_timeout_ms": 60000, "max_count_hits": 2000000,
               "max_hits": 500000, "threads": 1, "deny": ["transitive", "regex_no_prefix"]},
   "user":    {"slots": 4, "per_user": 2, "queue_ms": 10000,
               "timeout_ms": 60000, "total_timeout_ms": 300000, "max_count_hits": 20000000,
               "max_hits": 5000000, "threads": 4},
   "admin":   {"per_user": 4, "queue_ms": 30000, "threads": 8}},
 "pando": {"query_threads": 4, "session_max_hits": 5000000, "session_memory_mb": 4096}}
```

* FQS side — **admission** of heavy requests (a `/run` program, a `/query`
  with a `;` program): `slots` per tier and `heavy_slots` overall, waiting at
  most `queue_ms` (then **503** `{"busy": true, "retry_after_s": …}`), and
  `per_user` heavy requests at once per user (then **429**). The user is the
  token's `user` claim (without a secret: the `user` field), else the
  `session_id`, else the client address. Pages, `/status`, `/info`,
  `/context` and sessions never queue. Keep `heavy_slots × threads` at or
  below the cores.
* **Process slots** — every search that runs as a child process takes one of
  `process_slots` (default half the CPUs, at least 2), pages included, waiting
  at most `process_queue_ms` (default 30000, then **503** busy). That covers the
  CQP backend (`python -m flexicorp` + `cqp` per search) and pando without
  libflexicorp_pando (the cold CLI, `/health` → `pando.mode: "cli"`); FCS searchRetrieve too. A burst of CWB
  searches queues instead of starting one process each. Hot pando searches run
  inside FQS and never take one. `/health` → `limits.process_slots_free`.
* Engine side — the same tier objects go to pando, and FQS puts `"tier"` into
  every engine request (dropping any a client sent): `timeout_ms` (408; a
  request's own `timeout_ms` can only lower it), `total_timeout_ms`
  (background totals stop, `timed_out`), `max_count_hits` (413 for count /
  freq / coll / sort / … over more hits), `max_hits` (sorted sets), `threads`,
  `deny` (403 `{"denied": "<feature>"}`: `transitive`, `unbounded_repeat`,
  `regex_no_prefix`, `regex`, `parallel`, `negated_relation`). See pando's
  wiki CLI-Reference, "Limits by tier".
* Engine errors keep their status (403 / 408 / 413 / 404 / 429 / 503); the
  body is the engine's JSON.

**Per corpus**: a catalog entry's `settings.limits` overrides the file for that
corpus's engine — `{"tiers": {"visitor": {"max_count_hits": 500000}}, "pando":
{"query_threads": 8}}` (tier members replaced one by one). A warm corpus keeps the
options it was opened with until FQS closes and reopens it.

`/health` → `limits` shows the tiers, free slots and the role trust mode.

## Access by group

A corpus can be restricted to groups, as KonText restricts corpora: its `settings.access` lists
the groups that may use it (`["anonymous"]`, `["authenticated"]`, or named groups; `fqs corpora
import` writes KonText's `access="…"` there). The caller's groups come from the front-end, in
its signed token: a `groups` claim (a list, or a comma-separated string), next to `role` and
`user`. Every caller is in `anonymous`, a signed-in one (role `user` or `admin`) also in
`authenticated`. A caller may use the corpus when the two share a group; a corpus without
`access` is open, as before. Only a verified token carries groups: without a secret nothing a
request says about itself is checked.

Whether this is applied: `access.mode` in the limits file (`{"access": {"mode": "report"}}`), or
env `FQS_ACCESS_MODE`; a corpus's `settings.access_mode` overrides it, so one corpus can be
switched on before the rest.

* `off` (default) — `access` is not looked at;
* `report` — decided, and every "would refuse" written to the activity log (event
  `access_report`: corpus, route, role, groups, the corpus's `access`, whether the token was
  verified, and the user as the log writes users), or to the service log without an activity
  log; the request is answered as before. For checking a rollout against what the front-ends
  decide before anything is refused;
* `enforce` — refused (403, the message names the groups), and left out of `/corpora`,
  `/labels` and FCS.

The check covers every route that reads a corpus (`/query`, `/run`, `/info`, `/context`,
`/fragment`, `/status`, `/session`, `/warm`). An admin caller passes (`access.admin_bypass`,
default `true`). `/health` → `access.mode`. Which groups may use a corpus is for admins: `/corpora`
leaves `settings.access` and `settings.access_mode` out for other callers.

### Which corpora may this caller use

For a front-end that takes its corpus permissions from FQS and enforces them itself (KonText on
LINDAT: its corpus list and `corpus_access`). The answer is the rule above **whatever the access
mode** (off, report or enforce, server-wide or per corpus), so FQS can stay in `report` for its
other clients:

* `GET /corpora?permitted=1` — only the corpora the caller may use. The same rows and filters as
  `/corpora` (`q=`, `tag=`, `facet=`, `frontend=`, `view=browse`, …), each with `"permitted": true`;
  `settings.access` still only for admins.
* `GET /corpora/{id}/permitted` — `{"ok": true, "corpus": "<id>", "permitted": true|false}`; 404
  (`"ok": false`) for an unknown id.

"May use": the corpus's http policy lets the caller query it (`disabled`: nobody; `auth_required`:
admins; and its `query` operation is on), and then its `access` groups as above (no `access`: open;
an admin passes when `admin_bypass`). The caller is only what a **verified token** says: its `groups`,
`anonymous`, and `authenticated` for role `user` / `admin`. Without one (no token, a bad or expired
one, or FQS without a secret) the caller is anonymous: no groups, not signed in, not admin, whatever
`request_role` says. Both are answered from the catalogue alone, and write nothing to the activity
log. (`/corpora/{id}/may-use` is the other question: what FQS itself does under its current mode.)

## Activity log

`fqs serve --activity-log FILE` (or `FQS_ACTIVITY_LOG`) writes one JSON object
per line, for looking back at what the server did. It is off unless given, and
rotates like the request log (`--log-max-bytes`, `--log-keep-files`).

| Event | Fields |
| --- | --- |
| `start` | pid, version, `max_warm`, `idle_ttl_secs`, whether tiers are configured |
| `query` | a `/query`, `/run` or FCS searchRetrieve: `endpoint`, `corpus`, `query`, `role`, `tier`, `user`, `status`, `elapsed_ms`, `queued_ms` (admission wait, heavy requests), `warm` (false = the corpus had to be opened, `open_ms`), `hits` / `total` when answered, `error` and `denied` / `limit` / `busy` / `timed_out` when refused |
| `warm_open` | `corpus`, `open_ms`, `warm` (open corpora now) |
| `warm_close` | `corpus`, `reason` (`lru`: room for another corpus; `idle`: idle TTL), `age_secs`, `idle_secs`, `requests` served |
| `warm_full` | no room could be made (all open corpora in use, or the oldest still counting): FQS goes over `--pando-max-warm` for a while |
| `warm_state` | every `--activity-state-secs` (default 300; 0 = none): the open corpora with age, idle time and requests, and the process's `rss_bytes` |
| `access_report` | access mode `report`: a request the group check would refuse (`corpus`, `route`, `role`, `groups`, `access`, `verified`, `user`); see "Access by group" |

`--activity-events queries|warm|all` (default all) picks what is logged.
`--activity-log-users hash|plain|none` (default hash): the user is the identity
the limits use (JWT `sub`, the request's `user`, else `session:…` or `ip:…`);
hashed, it is 12 hex digits of a salted SHA-256, comparable within one run, or
across runs with a fixed `--activity-salt` / `FQS_ACTIVITY_SALT`.

    fqs serve --activity-log /var/log/fqs/activity.jsonl
    jq -c 'select(.event=="warm_close")' /var/log/fqs/activity.jsonl

## Stored subcorpora

Named subcorpora are kept in FQS (table `subcorpora` of the catalogue database), so every front end
(KonText, TEITOK, the portal) sees the same ones. One belongs to a corpus and an owner (the caller's
user: the token's `user` / `uid`; without `--jwt-secret`, the request's `user`); the name is unique
per corpus and owner (1–64 letters, digits, `_`, `.`).

**A definition is a recipe**, resolved on the corpus's engine at creation (its size is stored) and
again on the first use after the corpus was reindexed (pando: its `index_id`; Manatee: its registry
and data files changed):

| kind | example |
| --- | --- |
| `regions` | `{"kind": "regions", "struct": "text", "conditions": [{"attr": "genre", "value": ["fiction", "poetry"]}, {"attr": "year", "op": "!=", "value": "1990"}]}`: the regions whose attributes match every condition (`op` `=` / `!=`, literal values; a list: any of them; no conditions: every region) |
| `documents` | `{"kind": "documents", "ids": ["d1", "d7"], "struct"?: "doc", "attr"?: "id"}` (default: pando and CWB `text`.`id`, Manatee the registry's `FULLREF`) |
| `query` | `{"kind": "query", "query": "[upos=\"NOUN\"]", "language"?: "pando-cql"}`: the positions of the hits, sent as written |
| `expression` | `{"kind": "expression", "expression": "(fiction | poetry) & 1990s - spoken"}`: other subcorpora of the corpus by name (the owner's own, else a public one); `&` binds tighter than `|` and `-` |

Routes (the corpus's own access rule applies first, as on `/query`):

- `GET /corpora/{id}/subcorpora` — the ones the caller may use (`?owner=` only that owner's)
- `POST /corpora/{id}/subcorpora` `{"name", "definition", "visibility"?, "groups"?, "description"?, "backend"?}`
  → 201 and the subcorpus: `{corpus, name, owner, visibility, groups, description, definition, tokens,
  ranges, encoding, resolved: {engine, against, at}, created_at, updated_at, warnings?}`. 409 for a name
  the owner already has, 400 for a definition that selects nothing (or is wrong), 403 without an owner.
  `warnings` tells the creator when it is very fragmented (many short pieces: slower to search).
- `GET /corpora/{id}/subcorpora/{name}` (`?owner=` when several of that name are visible)
- `PATCH /corpora/{id}/subcorpora/{name}` `{"visibility"?, "groups"?, "description"?, "owner"?}` — owner or admin
- `DELETE /corpora/{id}/subcorpora/{name}` (`?owner=`) — owner or admin; 409 while an expression uses it

**Visibility**, checked on every use: `private` (the owner), `groups` (callers sharing one of its
`groups`: the token's `groups`, plus `anonymous` / `authenticated` as under *Access by group*),
`public`. An admin sees all. **Who may publish** (`groups` / `public`) is a setting, not stored data:
`{"subcorpora": {"publish": "admins"}}` in the limits file, or env `FQS_SUBCORPUS_PUBLISH`:

* `admins` (default) — only admins;
* `group_owners` — also a caller to groups their token lists in `owned_groups`; `public` stays with admins;
* `any_user` — any signed-in caller (role `user`), to groups they are in, or public.

Making one private again is always allowed; one published under a looser rule stays published until
its owner or an admin withdraws it.

**Using one:** `"subcorpus": "<name>"` on `/query` and `/run`, `?subcorpus=<name>` on `/info`.
`subcorpus_mode`: `use` (default; the subcorpus becomes the corpus: statistics relative to it, `/info`
`result.size` / `result.tokens` are its size, the corpus's in `subcorpus.corpus_tokens`) or `within`
(a filter; statistics of the whole corpus). A hit counts only when all its tokens are inside (Manatee's
rule); the context shown is not cut. Answers add `subcorpus: {name, mode, tokens, owner}` at the top
level (additive).

Per engine:

* **pando** — FQS sends the recipe as a pando definition per request (`"subcorpus": {"definition",
  "mode", "name"}`; regions as `r:<text> :: r.text_genre = "fiction"`, expressions with their names
  expanded) and resolves it with pando's `POST /subcorpus`. Needs FQS's pando library built with a
  pando that has subcorpora (EF-12, pando 6221f62 or later).
* **native Manatee** — FQS writes a `.subc` file (Manatee's own format, by Manatee's
  `create_subcorpus`, as KonText's subcorpus form does: `<catalogue folder>/subcorpora/<corpus>/`, env
  `FQS_SUBCORPUS_DIR`) and runs the query on the corpus opened as that subcorpus (Manatee's
  `SubCorpus`). Expressions are joined in FQS over the range lists. Manatee's answers carry counts,
  so `use` and `within` give the same hits; `within` reports the subcorpus for the front end to
  divide by the whole corpus. Needs libfqs_manatee 0.7 (`manatee/build.sh`); test data and a check in
  `testdata/manatee-subcorpora/`.
* **CWB** — CQP's own subcorpora, on FQS's CQP route: each part a CQP query (regions as
  `<text> [] :: match.text_genre = "fiction" expand to text`), its ranges read with `dump`, joined in
  FQS, kept with `undump` + `save` in `<catalogue folder>/subcorpora/<corpus>/cwb/`; a query activates
  it first (`set DataDirectory …; CORPUS:Name;`), so CQP searches only inside it. Default documents:
  `text`.`id`. As on Manatee, `use` and `within` give the same hits. A TEITOK project queried through
  flexicorp's CQP route has no subcorpora (400). Test data: `testdata/manatee-subcorpora/build-cwb.sh`.

## Pando hit-set sessions

`/query` with `session_id` and `name` stores the result in that session
(pando creates the session on first use, same id); `/query` with
`session_id` + `from: "<name>"` pages the stored set (sorted sets in their
sorted order); `/run` with `session_id` runs commands on the stored sets
(`sort Q1 by lemma`, `count Q1 by lemma`, `coll Q1`, `size Q1`). Sessions live
in the corpus handle: when FQS closes an idle corpus, its sessions go too, and
a request then gets **404** `unknown_session` / `unknown_hitset` — run the query
again. `POST /session {corpus, session_id?, ttl_s?}`, `GET /session?corpus=&session_id=`,
`POST /session/close {corpus, session_id}`, `GET /sessions?corpus=`.

## Random samples and shuffled concordances (pando)

`/query` passes `sample` (a random N of the hits, in corpus order), `shuffle`
(the hits in a random order) and `seed` (the same seed: the same sample / order
on every page) to pando-server (KonText's *Random sample* / *Shuffle*). Both go
through every hit per request and are not stored in sessions.

## Notes

- `fqs query` currently supports `pando` and `cqp`; other backends return `not implemented yet`.
- The HTTP surface is intended as the primary control/query interface for UI integrations.

## License

MIT — see [`LICENSE`](LICENSE). Exception: the optional Manatee plug-in in [`manatee/`](manatee/) links manatee-open and is GPL-2.0 (FQS loads it at run time only when it is installed).

If you use fqs in research, please cite it: [`CITATION.cff`](CITATION.cff) has the reference (GitHub shows it as "Cite this repository").
