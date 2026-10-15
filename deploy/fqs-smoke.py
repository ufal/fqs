#!/usr/bin/env python3
"""Smoke test of an FQS: is it up, what does it hold, and does every corpus answer a query (cold, then
warm)? Works on any FQS: this machine's, the server itself, or a server through an SSH tunnel:

    ssh -N -L 18787:127.0.0.1:8787 USER@FQS-MACHINE
    python3 fqs/deploy/fqs-smoke.py http://127.0.0.1:18787 [--secret-file FILE] [--only ID,ID]
        [--role visitor|user|admin] [--query '[]'] [--no-fcs] [--no-warm] [--json]

Checks: GET /health; GET /corpora; per corpus a one-hit query twice (cold and warm: the gap is what
the file cache, NFS and the warm-up are about; the engine's own time, so a tunnel's latency does not
hide it, and the round trip) and POST /warm; the FCS endpoint (explain and a search). One row per
corpus: engine, ok, times, hits (≥ while the engine is still counting), the error if any. Manatee
corpora are queried when the FQS has native Manatee (FQS_MANATEE_LIB), else noted as KonText's.
Exit status 1 when anything failed.

--secret-file: a file with FQS_SECRET=… to sign as --role (default admin with a secret, else visitor,
unsigned). The secret is read, never printed.
"""
import argparse
import base64
import hashlib
import hmac
import json
import sys
import time
import urllib.error
import urllib.parse
import urllib.request


class Fqs:
    def __init__(self, url, secret, role):
        self.url, self.secret, self.role = url.rstrip("/"), secret, role

    def headers(self, corpus="catalog"):
        h = {"Accept": "application/json", "Content-Type": "application/json"}
        if self.secret:
            b64 = lambda b: base64.urlsafe_b64encode(b).rstrip(b"=").decode()
            now = int(time.time())
            head = b64(json.dumps({"alg": "HS256", "typ": "JWT"}).encode())
            body = b64(json.dumps({"iss": "teitok-flexicorp", "sub": corpus, "role": self.role, "user": "fqs-smoke", "iat": now, "exp": now + 120}).encode())
            sig = b64(hmac.new(self.secret.encode(), f"{head}.{body}".encode(), hashlib.sha256).digest())
            h["Authorization"] = f"Bearer {head}.{body}.{sig}"
        return h

    def call(self, method, path, body=None, corpus="catalog", timeout=120, raw=False):
        """(status, parsed JSON or text, ms)"""
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(self.url + path, data=data, method=method, headers=self.headers(corpus))
        t0 = time.time()
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                text, status = r.read().decode("utf-8", "replace"), r.status
        except urllib.error.HTTPError as e:
            text, status = e.read().decode("utf-8", "replace"), e.code
        except Exception as e:   # unreachable, timeout
            return 0, str(e), (time.time() - t0) * 1000
        ms = (time.time() - t0) * 1000
        if raw:
            return status, text, ms
        try:
            return status, json.loads(text), ms
        except ValueError:
            return status, text, ms


def find(payload, key, path=()):
    """All (path, value) pairs of `key` in a nested answer."""
    out = []
    if isinstance(payload, dict):
        for k, v in payload.items():
            if k == key and not isinstance(v, (dict, list)):
                out.append((path + (k,), v, payload))
            out += find(v, key, path + (k,))
    return out


def hits_of(payload):
    """(hit count, exact?) from the engine's own answer (pando: raw/…/page/total; a page count otherwise)."""
    found = [(p, v, parent) for p, v, parent in find(payload, "total") if str(v).isdigit()]
    found.sort(key=lambda x: (x[0][0] != "raw", "job" not in x[0], len(x[0])))   # the engine's job total first
    if not found:
        return None, False
    p, v, parent = found[0]
    return int(v), bool(parent.get("total_exact", True))


def engine_ms(payload):
    """The engine's own time for the query (elapsed_ms in its answer), without the network."""
    for p, v, _ in find(payload, "elapsed_ms"):
        if p[0] == "raw":
            try:
                return float(v)
            except (TypeError, ValueError):
                pass
    return None


def error_of(status, payload):
    if isinstance(payload, dict):
        errs = payload.get("errors") or (payload.get("result") or {}).get("errors") if isinstance(payload.get("result"), dict) else payload.get("errors")
        if errs:
            return str(errs[0])[:100]
        if payload.get("ok") is False:
            return str(payload.get("error") or payload.get("message") or "not ok")[:100]
    if status != 200:
        return f"HTTP {status}: {str(payload)[:90]}"
    return ""


NOT_HERE = "is not implemented in fqs query execution"   # Manatee corpora: KonText runs them, FQS lists them


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("url", nargs="?", default="http://127.0.0.1:8787")
    ap.add_argument("--secret-file")
    ap.add_argument("--role", choices=["visitor", "user", "admin"])
    ap.add_argument("--only", help="comma-separated corpus ids")
    ap.add_argument("--query", default="[]", help="the query per corpus (default [], one hit)")
    ap.add_argument("--no-fcs", action="store_true")
    ap.add_argument("--no-warm", action="store_true")
    ap.add_argument("--json", action="store_true", help="results as JSON")
    a = ap.parse_args()
    secret = ""
    if a.secret_file:
        for line in open(a.secret_file, encoding="utf-8"):
            if line.startswith("FQS_SECRET="):
                secret = line.split("=", 1)[1].strip().strip("\"'")
    fqs = Fqs(a.url, secret, a.role or ("admin" if secret else "visitor"))
    report = {"url": fqs.url, "role": fqs.role, "signed": bool(secret), "corpora": [], "fcs": None}
    failed = False

    st, health, ms = fqs.call("GET", "/health", timeout=10)
    report["health"] = health if isinstance(health, dict) else {"error": str(health)[:200]}
    if st != 200 or not isinstance(health, dict) or not health.get("ok"):
        print(f"FQS at {fqs.url} is not answering: {str(health)[:200]}")
        sys.exit(1)
    st, cat, _ = fqs.call("GET", "/corpora?include_noncurrent=true&request_role=" + fqs.role)
    rows = cat.get("corpora", []) if isinstance(cat, dict) else []
    only = {x.strip() for x in a.only.split(",")} if a.only else None
    rows = [r for r in rows if not only or r.get("id") in only]
    if not a.json:
        print(f"FQS {fqs.url}  version {health.get('version')} ({health.get('build')}), {health.get('server_name') or 'unnamed'}; "
              f"{len(rows)} corpora; as {fqs.role}{' (signed)' if secret else ''}\n")
        print(f"{'corpus':24s} {'engine':7s} {'ok':4s} {'engine ms':>17s} {'round trip':>10s} {'hits':>12s}  warm-up / note")
        print(f"{'':24s} {'':7s} {'':4s} {'cold':>8s} {'warm':>8s} {'warm ms':>10s}")

    for r in rows:
        cid = r.get("id", "")
        body = {"corpus": cid, "query": a.query, "language": "auto", "start": 0, "size": 1, "window": 3, "request_role": fqs.role}
        st1, p1, cold = fqs.call("POST", "/query", body, corpus=cid)
        st2, p2, warm = fqs.call("POST", "/query", body, corpus=cid)
        err = error_of(st1, p1) or error_of(st2, p2)
        skipped = NOT_HERE in str(p1)
        if skipped:
            err = ""
        wstat = ""
        if not a.no_warm and not skipped:
            sw, pw, _ = fqs.call("POST", "/warm", {"corpus": cid}, corpus=cid, timeout=10)
            wstat = (pw.get("status") if isinstance(pw, dict) else f"HTTP {sw}") or ""
        n, exact = hits_of(p2 if not err else p1)
        row = {"corpus": cid, "engine": r.get("preferred_backend"), "ok": not err, "skipped": skipped,
               "engine_cold_ms": engine_ms(p1), "engine_warm_ms": engine_ms(p2), "roundtrip_ms": round(warm),
               "hits": n, "hits_exact": exact, "warm": wstat, "error": err}
        report["corpora"].append(row)
        failed |= bool(err)
        if not a.json:
            ms = lambda x: "" if x is None else (f"{x:.1f}" if x < 10 else f"{x:.0f}")
            hits = "" if n is None else (f"{n:,}" if exact else f"≥{n:,}")
            note = "served by KonText, not by FQS (Manatee)" if skipped else (err or wstat)
            ok = "—" if skipped else ("yes" if row["ok"] else "NO")
            print(f"{cid[:24]:24s} {str(row['engine'])[:7]:7s} {ok:4s} {ms(row['engine_cold_ms']):>8s} {ms(row['engine_warm_ms']):>8s} "
                  f"{row['roundtrip_ms']:10d} {hits:>12s}  {note}")

    if not a.no_fcs:
        se, explain, ems = fqs.call("GET", "/fcs?operation=explain&version=1.2", raw=True)
        ss, search, sms = fqs.call("GET", "/fcs?operation=searchRetrieve&version=1.2&maximumRecords=1&query=" + urllib.parse.quote(a.query if a.query != "[]" else "a"), raw=True, timeout=180)
        fcs = {"explain_ok": se == 200 and "explainResponse" in str(explain), "explain_ms": round(ems),
               "search_ok": ss == 200 and "searchRetrieveResponse" in str(search), "search_ms": round(sms),
               "records": (str(search).split("<sru:numberOfRecords>")[1].split("<")[0] if "<sru:numberOfRecords>" in str(search) else None)}
        report["fcs"] = fcs
        failed |= not (fcs["explain_ok"] and fcs["search_ok"])
        if not a.json:
            print(f"\nFCS: explain {'ok' if fcs['explain_ok'] else 'FAILED'} ({fcs['explain_ms']} ms), "
                  f"search {'ok' if fcs['search_ok'] else 'FAILED'} ({fcs['search_ms']} ms, {fcs['records']} records)")

    if a.json:
        print(json.dumps(report, ensure_ascii=False, indent=1))
    elif rows:
        bad = [c["corpus"] for c in report["corpora"] if not c["ok"]]
        skip = sum(1 for c in report["corpora"] if c.get("skipped"))
        asked = len(rows) - skip
        print(f"\n{asked - len(bad)} of {asked} corpora answer" + (f"; failing: {', '.join(bad)}" if bad else "")
              + (f"; {skip} served by KonText" if skip else "")
              + "\nengine ms: the engine's own time (cold = first query, warm = right after); round trip: as seen from here")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
