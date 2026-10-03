#!/usr/bin/env bash
# Install FQS as a system service: binary, config, admin UI, dirs, systemd unit.
#
# Usage (from repo or this directory):
#   sudo ./install.sh
#   sudo ./install.sh --teitok-venv /var/www/html/teitok/shared/Resources/venv
#   sudo ./install.sh --skip-build --web-user www-data
#
# See README «Installation» and deploy/fqs.env.example.
set -euo pipefail

PREFIX=/usr/local
UNIT_DIR=/etc/systemd/system
CONFIG_DIR=/etc/fqs
STATE_DIR=/var/lib/fqs
LOG_DIR=/var/log/fqs
SHARE_DIR=
DOC_DIR=
SERVICE_USER=fqs
SERVICE_GROUP=fqs
WEB_USER=www-data
TEITOK_VENV=
SKIP_BUILD=0
NO_SYSTEMD=0
ENABLE_NOW=1
SCAN_ROOTS=()

usage() {
	sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
	cat <<EOF

Options:
  --prefix DIR          Install prefix (default: /usr/local)
  --user NAME           Service user (default: fqs)
  --group NAME          Service group (default: fqs)
  --web-user NAME       PHP/TEITOK user added to service group (default: www-data)
  --teitok-venv DIR     TEITOK/flexicorp venv; sets PYTHON_BIN in fqs.env
  --scan-root DIR       Add a scan_roots entry (repeatable)
  --skip-build          Use existing target/release/fqs (do not cargo build)
  --no-systemd          Install files only; do not enable/start unit
  --no-start            Install + enable unit but do not start now
  -h, --help            Show this help
EOF
}

log() { printf '+ %s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

need_root() {
	if [[ "$(id -u)" -ne 0 ]]; then
		die "run as root (e.g. sudo $0 …)"
	fi
}

while [[ $# -gt 0 ]]; do
	case "$1" in
		--prefix) PREFIX=${2:?}; shift 2 ;;
		--user) SERVICE_USER=${2:?}; shift 2 ;;
		--group) SERVICE_GROUP=${2:?}; shift 2 ;;
		--web-user) WEB_USER=${2:?}; shift 2 ;;
		--teitok-venv) TEITOK_VENV=${2:?}; shift 2 ;;
		--scan-root) SCAN_ROOTS+=("${2:?}"); shift 2 ;;
		--skip-build) SKIP_BUILD=1; shift ;;
		--no-systemd) NO_SYSTEMD=1; shift ;;
		--no-start) ENABLE_NOW=0; shift ;;
		-h|--help) usage; exit 0 ;;
		*) die "unknown option: $1 (try --help)" ;;
	esac
done

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
FQS_ROOT=$SCRIPT_DIR
# Allow invoking as flexicorp/fqs/install.sh
if [[ ! -f "$FQS_ROOT/Cargo.toml" ]]; then
	die "Cargo.toml not found next to install.sh (expected fqs crate root)"
fi

SHARE_DIR=${SHARE_DIR:-$PREFIX/share/fqs}
DOC_DIR=${DOC_DIR:-$PREFIX/share/doc/fqs}
BIN_DIR=$PREFIX/bin
DEPLOY_DIR=$FQS_ROOT/deploy

need_root

if [[ ${#SCAN_ROOTS[@]} -eq 0 ]]; then
	for cand in /var/www/html/teitok /srv/teitok /data/corpora; do
		[[ -d "$cand" ]] && SCAN_ROOTS+=("$cand")
	done
fi
if [[ ${#SCAN_ROOTS[@]} -eq 0 ]]; then
	SCAN_ROOTS+=(/srv/teitok)
fi

# ── build ───────────────────────────────────────────────────────────────────
if [[ "$SKIP_BUILD" -eq 0 ]]; then
	command -v cargo >/dev/null || die "cargo not found; install Rust or pass --skip-build"
	log "building release binary"
	( cd "$FQS_ROOT" && cargo build --release )
else
	[[ -x "$FQS_ROOT/target/release/fqs" ]] || die "missing $FQS_ROOT/target/release/fqs (--skip-build)"
fi

# ── users / groups ───────────────────────────────────────────────────────────
if ! getent group "$SERVICE_GROUP" >/dev/null; then
	log "creating group $SERVICE_GROUP"
	groupadd --system "$SERVICE_GROUP"
fi
if ! id -u "$SERVICE_USER" >/dev/null 2>&1; then
	log "creating user $SERVICE_USER"
	useradd --system --gid "$SERVICE_GROUP" --home-dir "$STATE_DIR" \
		--shell /usr/sbin/nologin --comment "FQS service" "$SERVICE_USER"
fi
if id -u "$WEB_USER" >/dev/null 2>&1; then
	if ! id -nG "$WEB_USER" | tr ' ' '\n' | grep -qx "$SERVICE_GROUP"; then
		log "adding $WEB_USER to group $SERVICE_GROUP (TEITOK/PHP catalog writes)"
		usermod -aG "$SERVICE_GROUP" "$WEB_USER"
	fi
else
	log "warning: web user $WEB_USER not found — skip group membership"
fi

# ── directories ───────────────────────────────────────────────────────────────────
log "creating $CONFIG_DIR $STATE_DIR $LOG_DIR $SHARE_DIR"
install -d -m 0755 "$BIN_DIR" "$SHARE_DIR/admin" "$DOC_DIR"
install -d -m 2775 -o "$SERVICE_USER" -g "$SERVICE_GROUP" "$STATE_DIR" "$LOG_DIR"
install -d -m 0755 "$CONFIG_DIR"
# setgid so new DB/WAL files stay group-writable
chmod 2775 "$STATE_DIR" "$LOG_DIR"

# ── binary + admin UI + docs ─────────────────────────────────────────────────
log "installing $BIN_DIR/fqs"
install -m 0755 "$FQS_ROOT/target/release/fqs" "$BIN_DIR/fqs"

log "installing admin UI → $SHARE_DIR/admin"
install -m 0644 "$FQS_ROOT/admin/index.html" "$FQS_ROOT/admin/admin.css" "$FQS_ROOT/admin/app.js" \
	"$SHARE_DIR/admin/"

install -m 0644 "$FQS_ROOT/README.md" "$DOC_DIR/README.md"
install -m 0644 "$DEPLOY_DIR/fqs.env.example" "$DOC_DIR/fqs.env.example"
install -m 0644 "$DEPLOY_DIR/fqs.service" "$DOC_DIR/fqs.service"

# ── /etc/fqs/fqs.json ────────────────────────────────────────────────────────
if [[ ! -f "$CONFIG_DIR/fqs.json" ]]; then
	log "writing $CONFIG_DIR/fqs.json"
	{
		printf '{\n  "db_path": "%s/fqs.db",\n  "scan_roots": [\n' "$STATE_DIR"
		_i=0
		_n=${#SCAN_ROOTS[@]}
		for _root in "${SCAN_ROOTS[@]}"; do
			printf '    "%s"' "$_root"
			_i=$((_i + 1))
			[[ "$_i" -lt "$_n" ]] && printf ','
			printf '\n'
		done
		cat <<EOF
  ],
  "fqs": {
    "update_check": true,
    "restart": { "method": "systemctl", "unit": "fqs" }
  }
}
EOF
	} >"$CONFIG_DIR/fqs.json"
	chown root:"$SERVICE_GROUP" "$CONFIG_DIR/fqs.json"
	chmod 0640 "$CONFIG_DIR/fqs.json"
else
	log "keeping existing $CONFIG_DIR/fqs.json"
fi

# ── /etc/fqs/fqs.env ─────────────────────────────────────────────────────────
ENV_FILE=$CONFIG_DIR/fqs.env
if [[ ! -f "$ENV_FILE" ]]; then
	log "writing $ENV_FILE"
	cp "$DEPLOY_DIR/fqs.env.example" "$ENV_FILE"
	SECRET=$(command -v openssl >/dev/null && openssl rand -hex 32 || head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')
	# portable in-place replace for FQS_SECRET=
	tmp=$(mktemp)
	awk -v s="$SECRET" '
		BEGIN { done=0 }
		/^FQS_SECRET=/ && !done { print "FQS_SECRET=" s; done=1; next }
		{ print }
		END { if (!done) print "FQS_SECRET=" s }
	' "$ENV_FILE" >"$tmp"
	mv "$tmp" "$ENV_FILE"
else
	log "keeping existing $ENV_FILE"
fi

if [[ -n "$TEITOK_VENV" ]]; then
	PY=$TEITOK_VENV/bin/python
	[[ -x "$PY" ]] || die "teitok venv python not executable: $PY"
	if ! "$PY" -c 'import flexicorp' 2>/dev/null; then
		log "warning: $PY cannot import flexicorp — install the package into that venv"
		log "  $PY -m pip install -e $(cd "$FQS_ROOT/.." && pwd)"
	fi
	tmp=$(mktemp)
	awk -v p="$PY" '
		BEGIN { done=0 }
		/^PYTHON_BIN=/ && !done { print "PYTHON_BIN=" p; done=1; next }
		{ print }
		END { if (!done) print "PYTHON_BIN=" p }
	' "$ENV_FILE" >"$tmp"
	mv "$tmp" "$ENV_FILE"
	log "PYTHON_BIN=$PY"
elif [[ -x /var/www/html/teitok/shared/Resources/venv/bin/python ]]; then
	PY=/var/www/html/teitok/shared/Resources/venv/bin/python
	tmp=$(mktemp)
	awk -v p="$PY" '
		BEGIN { done=0 }
		/^#?PYTHON_BIN=/ && !done { print "PYTHON_BIN=" p; done=1; next }
		{ print }
		END { if (!done) print "PYTHON_BIN=" p }
	' "$ENV_FILE" >"$tmp"
	mv "$tmp" "$ENV_FILE"
	log "auto-detected PYTHON_BIN=$PY"
fi

chown root:"$SERVICE_GROUP" "$ENV_FILE"
chmod 0640 "$ENV_FILE"

# ensure catalog file exists and is group-writable
if [[ ! -f "$STATE_DIR/fqs.db" ]]; then
	log "initializing empty catalog $STATE_DIR/fqs.db"
	sudo -u "$SERVICE_USER" env FQS_DB_PATH="$STATE_DIR/fqs.db" "$BIN_DIR/fqs" corpora list >/dev/null || true
fi
chown "$SERVICE_USER":"$SERVICE_GROUP" "$STATE_DIR" "$LOG_DIR" 2>/dev/null || true
chown "$SERVICE_USER":"$SERVICE_GROUP" "$STATE_DIR/fqs.db" 2>/dev/null || true
chmod 664 "$STATE_DIR/fqs.db" 2>/dev/null || true
chmod 2775 "$STATE_DIR" "$LOG_DIR"

# ── systemd ──────────────────────────────────────────────────────────────────
if [[ "$NO_SYSTEMD" -eq 1 ]]; then
	log "skipping systemd (--no-systemd)"
else
	command -v systemctl >/dev/null || die "systemctl not found; pass --no-systemd on non-systemd hosts"
	UNIT_SRC=$DEPLOY_DIR/fqs.service
	UNIT_DST=$UNIT_DIR/fqs.service
	log "installing $UNIT_DST"
	# Rewrite User=/Group=/ paths if prefix or names differ from template defaults
	sed \
		-e "s|^User=.*|User=$SERVICE_USER|" \
		-e "s|^Group=.*|Group=$SERVICE_GROUP|" \
		-e "s|/usr/local/bin/fqs|$BIN_DIR/fqs|g" \
		-e "s|/usr/local/share/fqs/admin|$SHARE_DIR/admin|g" \
		"$UNIT_SRC" >"$UNIT_DST"
	# Drop-in for extra corpus roots (ProtectSystem=strict needs ReadWritePaths)
	DROP_IN_DIR=$UNIT_DIR/fqs.service.d
	install -d -m 0755 "$DROP_IN_DIR"
	{
		echo '[Service]'
		for _root in "${SCAN_ROOTS[@]}"; do
			echo "ReadWritePaths=$_root"
		done
	} >"$DROP_IN_DIR/scan-roots.conf"
	systemctl daemon-reload
	systemctl enable fqs.service
	if [[ "$ENABLE_NOW" -eq 1 ]]; then
		log "starting fqs.service"
		systemctl restart fqs.service
		systemctl --no-pager --full status fqs.service || true
	else
		log "enabled fqs.service (not started; --no-start)"
	fi
fi

cat <<EOF

FQS installed.

  binary:   $BIN_DIR/fqs  ($("$BIN_DIR/fqs" --version 2>/dev/null || echo version unknown))
  config:   $CONFIG_DIR/fqs.json
  env:      $ENV_FILE   (FQS_SECRET, PYTHON_BIN, …)
  catalog:  $STATE_DIR/fqs.db   (owner $SERVICE_USER:$SERVICE_GROUP, mode 2775 dir)
  admin UI: $SHARE_DIR/admin  → http://127.0.0.1:8790/admin/
  logs:     $LOG_DIR/

Next steps:
  1. Edit $ENV_FILE (PYTHON_BIN must point at a venv with flexicorp).
  2. Ensure TEITOK corpora dirs are writable by $SERVICE_USER (or group $SERVICE_GROUP),
     e.g. chgrp -R $SERVICE_GROUP /var/www/html/teitok/migrantstories && chmod -R g+w …
  3. Register a corpus:
       sudo -u $SERVICE_USER $BIN_DIR/fqs corpora upsert-json --json '{...}'
     or use the admin UI with: $BIN_DIR/fqs admin-token --user ops
  4. If PHP still cannot write the catalog, restart php-fpm so group membership applies:
       systemctl restart php*-fpm
  5. systemctl status fqs && curl -sS http://127.0.0.1:8787/health

Flexicorp (Python) is not bundled; install into the TEITOK venv if needed:
  \$PYTHON_BIN -m pip install -e $(cd "$FQS_ROOT/.." && pwd)

EOF
