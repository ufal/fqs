/* FQS Admin UI v0 — no build step; talks to api/* relative to this page (works under /admin/ or /fqsadmin/). */
(function () {
  const TOKEN_KEY = "fqs_admin_jwt";

  const $ = (id) => document.getElementById(id);
  const banner = $("banner");
  const listEl = $("corpus-list");
  const listEmpty = $("list-empty");
  // Was id="filter" — that collides with TEITOK pages / browser autofill and can
  // leave a phantom query that empties the list ("No match") after a click.
  const filterEl = $("corpus-filter") || $("filter");
  const tokenEl = $("token");

  // Inside TEITOK (index.php?action=fqsadmin): TEITOK's login is the sign-in, and
  // TEITOK signs each API call server-side — no token in the browser. The page
  // carries the proxy URL, a CSRF value to send back, and the TEITOK user.
  const meta = (n) => {
    const m = document.querySelector('meta[name="' + n + '"]');
    return m ? m.getAttribute("content") || "" : "";
  };
  const PROXY = meta("fqs-admin-proxy");
  const PROXY_CSRF = meta("fqs-admin-csrf");
  const PROXY_USER = meta("fqs-admin-user");

  /** URL of an admin API call: `path` like "/corpora/x?full=1". */
  function apiUrl(path) {
    if (!PROXY) return apiBase() + path;
    const q = path.indexOf("?");
    const p = q < 0 ? path : path.slice(0, q);
    const qs = q < 0 ? "" : path.slice(q + 1);
    return PROXY + "&fqsa=" + encodeURIComponent("api" + p) + (qs ? "&" + qs : "");
  }

  /** Admin API base ending in /api — derived from <base href> or the page URL. */
  function apiBase() {
    const base = document.baseURI || window.location.href;
    // …/fqsadmin/ or …/admin/ → …/api
    const page = base.endsWith("/") ? base : base.replace(/\/[^/]*$/, "/");
    return new URL("api", page).href.replace(/\/?$/, "");
  }
  let corpora = [];
  let selectedId = null;
  let draftNew = false;
  /** Browse labels currently on the open corpus form (order preserved). */
  let formLabels = [];
  /** Extra labels created via "Add new…" in this browser session (before/after save). */
  let sessionNewLabels = [];
  /** Corpus settings/capabilities kept in memory (shown read-only; not edited in the form). */
  let formSettings = {};
  let formCapabilities = {};

  /** Stored token → human label for the admin UI (catalogue still uses lang:/feature:). */
  const FEATURE_OPTIONS = [
    { token: "feature:spoken", title: "Spoken / audio" },
    { token: "feature:facsimile", title: "Facsimile images" },
    { token: "feature:video", title: "Video" },
    { token: "feature:parallel", title: "Parallel / aligned" },
    { token: "feature:geolocation", title: "Geolocation" },
    { token: "feature:dependencies", title: "Dependency trees" },
    { token: "feature:ner", title: "Named entities (NER)" },
    { token: "feature:ud", title: "Universal Dependencies" },
  ];
  const LANGUAGE_OPTIONS = [
    { token: "lang:cs", title: "Czech" },
    { token: "lang:en", title: "English" },
    { token: "lang:de", title: "German" },
    { token: "lang:nl", title: "Dutch" },
    { token: "lang:fr", title: "French" },
    { token: "lang:es", title: "Spanish" },
    { token: "lang:it", title: "Italian" },
    { token: "lang:pt", title: "Portuguese" },
    { token: "lang:pl", title: "Polish" },
    { token: "lang:sk", title: "Slovak" },
    { token: "lang:ru", title: "Russian" },
    { token: "lang:uk", title: "Ukrainian" },
    { token: "lang:hu", title: "Hungarian" },
    { token: "lang:fi", title: "Finnish" },
    { token: "lang:sv", title: "Swedish" },
    { token: "lang:da", title: "Danish" },
    { token: "lang:nb", title: "Norwegian" },
    { token: "lang:el", title: "Greek" },
    { token: "lang:tr", title: "Turkish" },
    { token: "lang:ar", title: "Arabic" },
    { token: "lang:zh", title: "Chinese" },
    { token: "lang:ja", title: "Japanese" },
    { token: "lang:ko", title: "Korean" },
    { token: "lang:la", title: "Latin" },
  ];
  const SUGGESTED_LABELS = FEATURE_OPTIONS.map((x) => x.token).concat(
    LANGUAGE_OPTIONS.map((x) => x.token)
  );

  function showBanner(msg, isErr) {
    banner.hidden = !msg;
    banner.textContent = msg || "";
    banner.classList.toggle("err", !!isErr);
  }

  function setListEmpty(html, visible) {
    listEmpty.hidden = !visible;
    if (visible) listEmpty.innerHTML = html;
  }

  function token() {
    if (PROXY) return "teitok-session";   // signed in through TEITOK; no token here
    return (tokenEl.value || sessionStorage.getItem(TOKEN_KEY) || "").trim();
  }

  function looksLikeJwt(t) {
    if (PROXY) return true;
    const parts = (t || "").split(".");
    return parts.length === 3 && parts.every((p) => p.length > 0);
  }

  function setAuthUi(signedIn, userHint) {
    const authBar = $("auth-bar");
    const sessionBar = $("session-bar");
    if (!authBar || !sessionBar) return;
    authBar.hidden = !!signedIn;
    sessionBar.hidden = !signedIn;
    const label = $("session-label");
    if (label) {
      label.textContent = PROXY
        ? "Signed in via TEITOK" + (PROXY_USER ? " as " + PROXY_USER : "")
        : userHint ? "Signed in as " + userHint : "Signed in";
    }
    const change = $("btn-change-token");
    if (change && PROXY) change.hidden = true;
  }

  function saveToken() {
    const t = tokenEl.value.trim();
    // Never persist admin JWTs across browser sessions (localStorage).
    try {
      localStorage.removeItem(TOKEN_KEY);
    } catch (_) {}
    if (t) sessionStorage.setItem(TOKEN_KEY, t);
    else sessionStorage.removeItem(TOKEN_KEY);
    if (!t) {
      setAuthUi(false);
      showBanner("Token cleared. Catalog stays hidden until you enter a JWT.", false);
      corpora = [];
      selectedId = null;
      renderList();
      return;
    }
    if (!looksLikeJwt(t)) {
      setAuthUi(false);
      showBanner(
        "That does not look like a JWT (need three dot-separated segments). Use: fqs admin-token",
        true
      );
      corpora = [];
      selectedId = null;
      renderList();
      return;
    }
    showBanner("Checking token…", false);
    refreshCorpora()
      .then(() => {
        setAuthUi(true);
        showBanner("", false);
      })
      .catch((e) => {
        setAuthUi(false);
        showBanner(e.message, true);
      });
  }

  async function api(path, opts = {}) {
    const headers = Object.assign({ Accept: "application/json" }, opts.headers || {});
    if (PROXY) {
      headers["X-FQS-Admin-CSRF"] = PROXY_CSRF;
    } else {
      const t = token();
      if (t) headers.Authorization = "Bearer " + t;
    }
    if (opts.body != null && !headers["Content-Type"]) {
      headers["Content-Type"] = "application/json";
    }
    const res = await fetch(apiUrl(path), Object.assign({ credentials: "same-origin" }, opts, { headers }));
    const text = await res.text();
    let data;
    if (res.ok && !text.trim()) {
      throw new Error("The admin API answered " + res.status + " with an empty body (" + apiUrl(path) + ")" +
        (PROXY ? " — open " + PROXY + "&fqsa=selftest to see what TEITOK and FQS report" : ""));
    }
    try {
      data = text ? JSON.parse(text) : null;
    } catch (_) {
      data = { ok: false, error: text || res.statusText };
      if (res.ok) {
        // a 200 that is not JSON (an HTML page, a proxy's answer): say so instead
        // of showing an empty catalog
        const snippet = String(text || "").replace(/\s+/g, " ").trim().slice(0, 160);
        throw new Error("The admin API answered with something that is not JSON (" +
          (res.headers.get("content-type") || "no content type") + "): " + snippet);
      }
    }
    if (!res.ok) {
      const err = (data && (data.error || data.message)) || res.statusText || String(res.status);
      if ((res.status === 401 || res.status === 403) && !PROXY) {
        setAuthUi(false);
      }
      throw new Error(err);
    }
    return data;
  }

  function corpusMatchesFilter(c, q) {
    if (!q) return true;
    return (
      (c.id || "").toLowerCase().includes(q) ||
      (c.label || "").toLowerCase().includes(q) ||
      (c.environment || "").toLowerCase().includes(q)
    );
  }

  /** Keep the open corpus visible; password managers sometimes autofill #corpus-filter. */
  function ensureSelectionVisible() {
    if (!filterEl || !selectedId) return;
    const c = corpora.find((x) => x.id === selectedId);
    if (!c) return;
    const q = (filterEl.value || "").trim().toLowerCase();
    if (q && !corpusMatchesFilter(c, q)) {
      filterEl.value = "";
    }
  }

  function renderList() {
    ensureSelectionVisible();
    const q = (filterEl && filterEl.value ? filterEl.value : "").trim().toLowerCase();
    listEl.innerHTML = "";
    const t = token();
    if (!t) {
      setListEmpty(
        "<strong>No catalog loaded</strong>" +
          "Enter an admin <em>JWT</em> above and click <em>Use token</em>. " +
          "The list stays empty until then (this is not FQS_SECRET — that secret only signs the token).",
        true
      );
      return;
    }
    if (!looksLikeJwt(t)) {
      setListEmpty(
        "<strong>Token looks wrong</strong>" +
          "A JWT has three parts separated by dots (<code>header.payload.sig</code>). " +
          "Do not paste <code>FQS_SECRET</code> here.",
        true
      );
      return;
    }
    let filtered = corpora.filter((c) => corpusMatchesFilter(c, q));
    // If a phantom filter hid everything but we have a selection, drop the filter once.
    if (
      !filtered.length &&
      q &&
      selectedId &&
      corpora.some((c) => c.id === selectedId)
    ) {
      filterEl.value = "";
      filtered = corpora.slice();
    }
    if (!corpora.length) {
      setListEmpty(
        "<strong>Authenticated, but catalog is empty</strong>" +
          "This FQS database has no corpora yet. Use <em>New</em> or upsert via the API.",
        true
      );
      return;
    }
    if (!filtered.length) {
      setListEmpty(
        "<strong>No match</strong>Nothing matches the current filter" +
          (q ? " (<code>" + esc(filterEl.value.trim()) + "</code>)." : ".") +
          ' <button type="button" class="secondary" id="btn-clear-corpus-filter">Clear filter</button>',
        true
      );
      const clearBtn = $("btn-clear-corpus-filter");
      if (clearBtn) {
        clearBtn.addEventListener("click", () => {
          filterEl.value = "";
          renderList();
        });
      }
      return;
    }
    setListEmpty("", false);
    filtered.forEach((c) => {
      const li = document.createElement("li");
      if (c.id === selectedId) li.classList.add("active");
      li.innerHTML = '<span class="id"></span><span class="sub"></span>';
      li.querySelector(".id").textContent = c.id;
      li.querySelector(".sub").textContent =
        (c.label || "") +
        " · " +
        (c.preferred_backend || "") +
        (c.is_current ? "" : " · superseded") +
        (c.last_validation_ok === false ? " · validate fail" : "");
      li.addEventListener("click", () => selectCorpus(c.id));
      listEl.appendChild(li);
    });
  }

  function emptyEntry() {
    return {
      id: "",
      label: "",
      project_root: "",
      project_url: "",
      preferred_backend: "auto",
      environment: "live",
      http_policy_mode: "public_query",
      listing_visibility: "public",
      is_current: true,
      supports_xml: false,
      labels: [],
      http_allowed_operations: ["query", "catalog"],
      settings: {},
      capabilities: {},
    };
  }

  function labelKey(s) {
    return String(s || "").trim().toLowerCase();
  }

  function normalizeLabelList(list) {
    const out = [];
    const seen = new Set();
    (list || []).forEach((raw) => {
      const t = String(raw || "").trim();
      if (!t) return;
      const k = labelKey(t);
      if (seen.has(k)) return;
      seen.add(k);
      out.push(t);
    });
    return out;
  }

  function parseLabelToken(raw) {
    const t = String(raw || "").trim();
    const m = t.match(/^(lang|language|feature|features)\s*:\s*(.+)$/i);
    if (m) {
      const g = m[1].toLowerCase().startsWith("lang") ? "lang" : "feature";
      return { group: g, value: m[2].trim().toLowerCase(), token: g + ":" + m[2].trim().toLowerCase() };
    }
    return { group: "other", value: t, token: t };
  }

  function friendlyLabel(raw) {
    const k = labelKey(raw);
    const feat = FEATURE_OPTIONS.find((x) => labelKey(x.token) === k);
    if (feat) return feat.title;
    const lang = LANGUAGE_OPTIONS.find((x) => labelKey(x.token) === k);
    if (lang) return lang.title;
    const p = parseLabelToken(raw);
    if (p.group === "lang") return "Language: " + p.value;
    if (p.group === "feature") return "Feature: " + p.value;
    return p.token;
  }

  function toggleTokenSet() {
    const set = new Set();
    FEATURE_OPTIONS.forEach((x) => set.add(labelKey(x.token)));
    LANGUAGE_OPTIONS.forEach((x) => set.add(labelKey(x.token)));
    extraLanguageOptions().forEach((x) => set.add(labelKey(x.token)));
    return set;
  }

  function catalogLabelVocabulary() {
    const seen = new Set();
    const out = [];
    const push = (raw) => {
      const t = String(raw || "").trim();
      if (!t) return;
      const k = labelKey(t);
      if (seen.has(k)) return;
      seen.add(k);
      out.push(t);
    };
    corpora.forEach((c) => (c.labels || []).forEach(push));
    sessionNewLabels.forEach(push);
    SUGGESTED_LABELS.forEach(push);
    formLabels.forEach(push);
    out.sort((a, b) => a.localeCompare(b, undefined, { sensitivity: "base" }));
    return out;
  }

  /** Extra languages already on this corpus / catalogue but not in the fixed list. */
  function extraLanguageOptions() {
    const known = new Set(LANGUAGE_OPTIONS.map((x) => labelKey(x.token)));
    const out = [];
    const seen = new Set();
    const consider = (raw) => {
      const p = parseLabelToken(raw);
      if (p.group !== "lang") return;
      if (known.has(labelKey(p.token)) || seen.has(labelKey(p.token))) return;
      seen.add(labelKey(p.token));
      out.push({ token: p.token, title: "Language: " + p.value });
    };
    formLabels.forEach(consider);
    corpora.forEach((c) => (c.labels || []).forEach(consider));
    out.sort((a, b) => a.title.localeCompare(b.title));
    return out;
  }

  function setFormLabels(list) {
    formLabels = normalizeLabelList(list);
    renderLabelsEditor();
  }

  function toggleManagedLabel(token, on) {
    const k = labelKey(token);
    if (on) {
      if (!formLabels.some((x) => labelKey(x) === k)) formLabels.push(token);
    } else {
      formLabels = formLabels.filter((x) => labelKey(x) !== k);
    }
    renderLabelsEditor();
  }

  function renderToggleGroup(hostId, options) {
    const host = $(hostId);
    if (!host) return;
    const selected = new Set(formLabels.map(labelKey));
    host.innerHTML = "";
    options.forEach((opt) => {
      const lab = document.createElement("label");
      const cb = document.createElement("input");
      cb.type = "checkbox";
      cb.checked = selected.has(labelKey(opt.token));
      cb.addEventListener("change", () => toggleManagedLabel(opt.token, cb.checked));
      lab.appendChild(cb);
      lab.appendChild(document.createTextNode(opt.title));
      host.appendChild(lab);
    });
  }

  function renderLabelsEditor() {
    const box = $("labels-selected");
    const pick = $("labels-pick");
    const newRow = $("labels-new-row");
    if (!box || !pick) return;

    renderToggleGroup("labels-langs", LANGUAGE_OPTIONS.concat(extraLanguageOptions()));
    renderToggleGroup("labels-features", FEATURE_OPTIONS);

    const toggled = toggleTokenSet();
    const other = formLabels.filter((l) => !toggled.has(labelKey(l)));
    box.innerHTML = "";
    if (!other.length) {
      const empty = document.createElement("span");
      empty.className = "empty";
      empty.textContent = "None";
      box.appendChild(empty);
    } else {
      other.forEach((lab) => {
        const chip = document.createElement("span");
        chip.className = "labels-chip";
        const text = document.createElement("span");
        text.textContent = friendlyLabel(lab);
        text.title = lab;
        const rm = document.createElement("button");
        rm.type = "button";
        rm.setAttribute("aria-label", "Remove " + lab);
        rm.textContent = "×";
        rm.addEventListener("click", () => {
          formLabels = formLabels.filter((x) => labelKey(x) !== labelKey(lab));
          renderLabelsEditor();
        });
        chip.appendChild(text);
        chip.appendChild(rm);
        box.appendChild(chip);
      });
    }

    const selected = new Set(formLabels.map(labelKey));
    const available = catalogLabelVocabulary().filter(
      (l) => !selected.has(labelKey(l)) && !toggled.has(labelKey(l))
    );
    pick.innerHTML = "";
    const ph = document.createElement("option");
    ph.value = "";
    ph.textContent = available.length ? "Select a tag…" : "No other tags in catalogue";
    pick.appendChild(ph);
    available.forEach((lab) => {
      const opt = document.createElement("option");
      opt.value = lab;
      opt.textContent = friendlyLabel(lab);
      opt.title = lab;
      pick.appendChild(opt);
    });
    pick.disabled = !available.length;
    pick.value = "";

    if (newRow) newRow.hidden = true;
    const inp = $("labels-new-input");
    if (inp) inp.value = "";
  }

  function addExistingLabel(raw) {
    const t = String(raw || "").trim();
    if (!t) return;
    if (formLabels.some((x) => labelKey(x) === labelKey(t))) return;
    formLabels.push(t);
    renderLabelsEditor();
  }

  function addNewLabel(raw) {
    let t = String(raw || "").trim();
    if (!t) throw new Error("Enter a non-empty tag");
    if (/[,\n\r]/.test(t)) throw new Error("One tag at a time (no commas)");
    if (t.length > 80) throw new Error("Tag is too long");
    // Bare 2–3 letter codes → lang:xx
    if (/^[A-Za-z]{2,3}$/.test(t) && !t.includes(":")) {
      t = "lang:" + t.toLowerCase();
    }
    const k = labelKey(t);
    if (formLabels.some((x) => labelKey(x) === k)) {
      throw new Error("Already on this corpus");
    }
    const known = catalogLabelVocabulary().find((x) => labelKey(x) === k);
    const final = known || t;
    if (!known && !sessionNewLabels.some((x) => labelKey(x) === k)) {
      sessionNewLabels.push(final);
    }
    formLabels.push(final);
    renderLabelsEditor();
  }

  function fillForm(c) {
    $("f-id").value = c.id || "";
    $("f-id").readOnly = !draftNew && !!c.id;
    $("f-label").value = c.label || "";
    $("f-project_root").value = c.project_root || "";
    $("f-project_url").value = c.project_url || "";
    $("f-preferred_backend").value = c.preferred_backend || "auto";
    $("f-environment").value = c.environment || "live";
    $("f-http_policy_mode").value = c.http_policy_mode || "public_query";
    $("f-listing_visibility").value = c.listing_visibility || "public";
    $("f-is_current").checked = c.is_current !== false;
    $("f-supports_xml").checked = !!c.supports_xml;
    setFormLabels(c.labels || []);
    $("f-ops").value = (c.http_allowed_operations || []).join(", ");
    formSettings =
      c.settings && typeof c.settings === "object" && !Array.isArray(c.settings)
        ? c.settings
        : {};
    formCapabilities =
      c.capabilities && typeof c.capabilities === "object" && !Array.isArray(c.capabilities)
        ? c.capabilities
        : {};
    const settingsOut = $("f-settings-out");
    const capsOut = $("f-capabilities-out");
    if (settingsOut) settingsOut.textContent = JSON.stringify(formSettings, null, 2);
    if (capsOut) capsOut.textContent = JSON.stringify(formCapabilities, null, 2);
    $("meta").textContent = [
      c.corpus_size != null ? "corpus_size: " + c.corpus_size : null,
      c.last_validated_at ? "last_validated_at: " + c.last_validated_at : null,
      c.last_validation_ok != null ? "last_validation_ok: " + c.last_validation_ok : null,
      c.last_validation_message ? "message: " + c.last_validation_message : null,
      c.created_at ? "created_at: " + c.created_at : null,
      c.updated_at ? "updated_at: " + c.updated_at : null,
    ]
      .filter(Boolean)
      .join("\n");
    $("validate-out").hidden = true;
  }

  function readForm() {
    const split = (s) =>
      (s || "")
        .split(",")
        .map((x) => x.trim())
        .filter(Boolean);
    const id = $("f-id").value.trim();
    if (!id) throw new Error("id is required");
    const root = $("f-project_root").value.trim();
    if (!root) throw new Error("project_root is required");
    return {
      id,
      label: $("f-label").value.trim() || id,
      project_root: root,
      project_url: $("f-project_url").value.trim() || null,
      preferred_backend: $("f-preferred_backend").value,
      environment: $("f-environment").value.trim() || "live",
      http_policy_mode: $("f-http_policy_mode").value,
      listing_visibility: $("f-listing_visibility").value.trim() || "public",
      is_current: $("f-is_current").checked,
      supports_xml: $("f-supports_xml").checked,
      labels: normalizeLabelList(formLabels),
      http_allowed_operations: split($("f-ops").value),
      // Preserve blobs loaded with the form; do not accept free-form JSON edits here.
      settings: formSettings && typeof formSettings === "object" ? formSettings : {},
      capabilities:
        formCapabilities && typeof formCapabilities === "object" ? formCapabilities : {},
    };
  }

  function selectCorpus(id) {
    draftNew = false;
    selectedId = id;
    const c = corpora.find((x) => x.id === id);
    // Fill first: browser/password-manager autofill can then rewrite #corpus-filter.
    if (c) fillForm(c);
    ensureSelectionVisible();
    renderList();
  }

  async function refreshCorpora() {
    showBanner("", false);
    const data = await api("/corpora");
    corpora = data.corpora || [];
    renderList();
    if (selectedId && corpora.some((c) => c.id === selectedId)) {
      selectCorpus(selectedId);
    } else if (corpora.length && !draftNew) {
      selectCorpus(corpora[0].id);
    }
  }

  function esc(s) {
    return String(s == null ? "" : s)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#39;");
  }

  function stat(k, v, cls) {
    return (
      '<div class="stat"><span class="k">' +
      esc(k) +
      '</span><span class="v' +
      (cls ? " " + cls : "") +
      '">' +
      v +
      "</span></div>"
    );
  }

  function statusPill(status) {
    const s = String(status || "").toLowerCase();
    let cls = "";
    if (s === "finished" || s === "completed" || s === "ok" || s === "healthy") cls = "ok";
    else if (s === "failed" || s === "error" || s === "down" || s === "missing") cls = "bad";
    else if (s === "running") cls = "run";
    else if (s === "queued" || s === "configured" || s === "catalog" || s === "warn") cls = "warn";
    return '<span class="pill ' + cls + '">' + esc(status || "—") + "</span>";
  }

  function renderHealth(data) {
    const pando = data.pando || {};
    const eng = pando.engine || {};
    const limits = data.limits || {};
    const warm = Array.isArray(pando.warm) ? pando.warm : [];
    const pandoOk = pando.available !== false && (pando.api_version || eng.version);
    const trust = limits.role_trust || "—";
    const slots =
      limits.process_slots_free != null
        ? limits.process_slots_free + " / " + (limits.process_slots ?? "?")
        : "—";
    const heavy =
      limits.heavy_slots > 0
        ? (limits.heavy_slots_free ?? "?") + " / " + limits.heavy_slots
        : "off";
    const selfInfo = data.fqs || {};
    const upd = selfInfo.update || {};

    let html = '<h3 class="section-title">This FQS</h3>';
    html += '<div class="stat-grid">';
    html += stat("Version", esc(selfInfo.version || data.version || "—"), "ok");
    html += stat("PID", esc(selfInfo.pid != null ? selfInfo.pid : "—"), "muted");
    html += stat("Server name", esc(selfInfo.server_name || data.server_name || "—"), "");
    if (upd.disabled) {
      html += stat("Updates", "check disabled", "muted");
    } else if (upd.update_available) {
      html += stat(
        "Updates",
        "available: " + esc(upd.latest) + " (running " + esc(upd.local) + ")",
        "bad"
      );
    } else if (upd.checked && upd.latest) {
      html += stat("Updates", "up to date (" + esc(upd.latest) + ")", "ok");
    } else if (upd.error) {
      html += stat("Updates", "check failed", "warn");
    } else {
      html += stat("Updates", "—", "muted");
    }
    html += stat(
      "Restart",
      selfInfo.restartable ? "configured" : "not configured",
      selfInfo.restartable ? "ok" : "muted"
    );
    html += "</div>";
    if (upd.error && !upd.disabled) {
      html +=
        '<p class="muted muted-sm">Update check: ' +
        esc(upd.error) +
        (upd.url ? " · " + esc(upd.url) : "") +
        "</p>";
    } else if (upd.url && upd.checked) {
      html +=
        '<p class="muted muted-sm">Compared with <span class="mono">' +
        esc(upd.url) +
        "</span></p>";
    }
    if (selfInfo.restart_policy) {
      html +=
        '<p class="muted muted-sm">' + esc(selfInfo.restart_policy) + "</p>";
    }
    const restartBtn = $("btn-fqs-restart");
    if (restartBtn) {
      restartBtn.hidden = !selfInfo.restartable;
    }

    html += '<h3 class="section-title">Catalog & admission</h3>';
    html += '<div class="stat-grid">';
    html += stat("Service", esc(data.service || "fqs") + " " + esc(data.version || ""), data.ok ? "ok" : "bad");
    html += stat("Catalog corpora", esc(data.catalog_corpora), "");
    html += stat("Admin HTTP", data.admin_http ? "enabled" : "off", data.admin_http ? "ok" : "muted");
    html += stat("Role trust", esc(trust), trust === "jwt" ? "ok" : "bad");
    html += stat("Process slots free", esc(slots), "");
    html += stat("Heavy slots free", esc(heavy), "");
    html += stat("DB source", esc(data.db_source || "—"), "muted");
    html += "</div>";

    html += '<p class="muted mono db-path-line">' +
      "db: " + esc(data.db_path || "—") + "</p>";

    html += '<h3 class="section-title">Pando hot path</h3>';
    if (!pandoOk) {
      html += '<p class="muted">Pando library unavailable — cold CLI fallback.</p>';
    } else {
      html += '<div class="stat-grid">';
      html += stat("API", esc(pando.api_version), "");
      html += stat("Engine", esc(eng.version || pando.engine_build || "—"), "");
      html += stat("Build", esc(eng.build || eng.build_string || "—"), "muted");
      html += stat("Warm", warm.length + " / " + (pando.max_warm ?? "?"), "");
      html += stat("Idle TTL", (pando.idle_ttl_secs ?? "—") + "s", "muted");
      html += stat("Engine options", pando.engine_options_applied ? "applied" : "not applied",
        pando.engine_options_applied ? "ok" : "bad");
      html += "</div>";
      if (eng.features && eng.features.length) {
        html += '<div class="chip-row chip-row-spaced">';
        eng.features.forEach((f) => { html += '<span class="chip">' + esc(f) + "</span>"; });
        html += "</div>";
      }
      if (warm.length) {
        html += '<table class="data-table"><thead><tr><th>Warm corpus</th><th>Idle</th><th>Age</th><th>Requests</th></tr></thead><tbody>';
        warm.forEach((w) => {
          const id =
            typeof w === "string"
              ? w
              : w.corpus_id || w.corpus || w.id || JSON.stringify(w);
          const idle =
            w.fqs_idle_secs != null
              ? w.fqs_idle_secs + "s"
              : w.idle_secs != null
                ? Math.round(w.idle_secs) + "s"
                : "—";
          html +=
            "<tr><td class='mono'>" +
            esc(id) +
            "</td><td>" +
            esc(idle) +
            "</td><td>" +
            esc(w.age_secs != null ? w.age_secs + "s" : "—") +
            "</td><td>" +
            esc(w.requests != null ? w.requests : "—") +
            "</td></tr>";
        });
        html += "</tbody></table>";
      } else {
        html += '<p class="muted">No warm corpora right now.</p>';
      }
    }

    const tiers = limits.tiers || {};
    const tierNames = Object.keys(tiers);
    html += '<h3 class="section-title">Admission / tiers</h3>';
    if (!tierNames.length) {
      html += '<p class="muted">No tier file loaded (limits file empty or absent).</p>';
    } else {
      html += '<table class="data-table"><thead><tr><th>Tier</th><th>Slots free</th><th>Per user</th><th>Queue ms</th></tr></thead><tbody>';
      tierNames.forEach((name) => {
        const t = tiers[name] || {};
        html +=
          "<tr><td>" +
          esc(name) +
          (name === limits.default_tier ? " <span class='pill'>default</span>" : "") +
          "</td><td>" +
          esc((t.slots_free != null ? t.slots_free : "—") + " / " + (t.slots ?? "—")) +
          "</td><td>" +
          esc(t.per_user ?? "—") +
          "</td><td>" +
          esc(t.queue_ms ?? "—") +
          "</td></tr>";
      });
      html += "</tbody></table>";
    }

    $("health-view").innerHTML = html;
    $("health-out").textContent = JSON.stringify(data, null, 2);
  }

  function renderReindex(data) {
    const jobs = data.jobs || [];
    const filter = data.status_filter || "—";
    let html = '<div class="stat-grid">';
    html += stat("Filter", esc(filter), "");
    html += stat("Jobs shown", String(jobs.length), "");
    const running = jobs.filter((j) => j.status === "running").length;
    const queued = jobs.filter((j) => j.status === "queued").length;
    const failed = jobs.filter((j) => j.status === "failed").length;
    html += stat("Running", String(running), running ? "ok" : "muted");
    html += stat("Queued", String(queued), queued ? "" : "muted");
    html += stat("Failed (in page)", String(failed), failed ? "bad" : "muted");
    html += "</div>";

    if (!jobs.length) {
      html += '<p class="muted">No jobs for this filter.</p>';
    } else {
      html +=
        '<table class="data-table"><thead><tr>' +
        "<th>Status</th><th>Corpus</th><th>Backends</th><th>Requested</th><th>Finished</th><th>Message</th>" +
        "</tr></thead><tbody>";
      jobs.forEach((j) => {
        const backends = (j.requested_backends || []).join(", ") || "—";
        const msg = j.last_error || j.message || "";
        const progress =
          j.result && j.result.progress
            ? " · " +
              (j.result.progress.percent != null ? j.result.progress.percent + "%" : "") +
              (j.result.progress.phase ? " " + j.result.progress.phase : "")
            : "";
        html +=
          "<tr>" +
          "<td>" +
          statusPill(j.status) +
          "</td>" +
          "<td class='mono'>" +
          esc(j.corpus_id) +
          "</td>" +
          "<td>" +
          esc(backends) +
          "</td>" +
          "<td class='mono'>" +
          esc(j.requested_at || "—") +
          "</td>" +
          "<td class='mono'>" +
          esc(j.finished_at || "—") +
          "</td>" +
          "<td class='" +
          (j.last_error ? "err" : "") +
          "'>" +
          esc(msg) +
          esc(progress) +
          "<div class='muted mono muted-xs-mt'>" +
          esc(j.job_id || "") +
          "</div></td>" +
          "</tr>";
      });
      html += "</tbody></table>";
    }

    $("reindex-view").innerHTML = html;
    $("reindex-out").textContent = JSON.stringify(data, null, 2);
  }

  async function refreshHealth() {
    if (!token()) {
      $("health-view").innerHTML =
        '<p class="list-empty"><strong>JWT required</strong>Health details load only after you enter an admin token.</p>';
      $("health-out").textContent = "—";
      return;
    }
    const data = await api("/health");
    renderHealth(data);
  }

  async function refreshReindex() {
    if (!token()) {
      $("reindex-view").innerHTML =
        '<p class="list-empty"><strong>JWT required</strong>Reindex jobs load only after you enter an admin token.</p>';
      $("reindex-out").textContent = "—";
      return;
    }
    const status = ($("reindex-status") && $("reindex-status").value) || "all";
    const data = await api("/reindex/jobs?status=" + encodeURIComponent(status) + "&limit=100");
    renderReindex(data);
  }

  function formatSettingValue(v) {
    if (v == null) return "—";
    if (typeof v === "boolean") return v ? "true" : "false";
    if (typeof v === "object") return JSON.stringify(v);
    return String(v);
  }

  function renderSettings(data) {
    const sections = data.sections || [];
    let html = "";
    if (data.policy) {
      html +=
        '<p class="settings-policy">' +
        esc(data.policy) +
        "</p>";
    }
    html += '<div class="stat-grid">';
    html += stat("Mutable here", data.mutable ? "yes" : "no — report only", data.mutable ? "warn" : "ok");
    html += stat("fqs.json", esc(data.fqs_config_path || "—"), "muted");
    html += "</div>";

    sections.forEach((sec) => {
      html += '<h3 class="section-title">' + esc(sec.title || sec.id || "Section") + "</h3>";
      if (sec.note) {
        html += '<p class="muted settings-note">' + esc(sec.note) + "</p>";
      }
      if (sec.change && !(sec.items && sec.items.length)) {
        html +=
          '<p class="muted mono settings-change">Change: ' +
          esc(sec.change) +
          "</p>";
      }
      const items = sec.items || [];
      if (items.length) {
        html +=
          '<table class="data-table settings-table"><thead><tr>' +
          "<th>Setting</th><th>Value</th><th>Source</th><th>How to change</th>" +
          "</tr></thead><tbody>";
        items.forEach((it) => {
          html +=
            "<tr>" +
            "<td class='mono'>" +
            esc(it.key || "—") +
            "</td>" +
            "<td class='mono'>" +
            esc(formatSettingValue(it.value)) +
            "</td>" +
            "<td class='muted'>" +
            esc(it.source || "—") +
            "</td>" +
            "<td class='muted settings-change-cell'>" +
            esc(it.change || "—") +
            "</td>" +
            "</tr>";
        });
        html += "</tbody></table>";
      }
      if (sec.id === "limits" && sec.config && Object.keys(sec.config).length) {
        html +=
          '<details class="raw-json limits-json"><summary>Limits file JSON</summary>' +
          '<pre class="out">' +
          esc(JSON.stringify(sec.config, null, 2)) +
          "</pre></details>";
      }
    });

    $("settings-view").innerHTML = html;
    $("settings-out").textContent = JSON.stringify(data, null, 2);
  }

  async function refreshSettings() {
    if (!token()) {
      $("settings-view").innerHTML =
        '<p class="list-empty"><strong>JWT required</strong>Settings load only after you enter an admin token.</p>';
      $("settings-out").textContent = "—";
      return;
    }
    const data = await api("/settings");
    renderSettings(data);
  }

  function activityDetail(ev) {
    const e = ev.event || "";
    if (e === "query") {
      const bits = [];
      if (ev.query) bits.push(ev.query);
      if (ev.endpoint) bits.push(ev.endpoint);
      if (ev.hits != null) bits.push("hits=" + ev.hits);
      if (ev.total != null) bits.push("total=" + ev.total);
      if (ev.elapsed_ms != null) bits.push(ev.elapsed_ms + "ms");
      if (ev.error) bits.push("err: " + ev.error);
      if (ev.busy) bits.push("busy");
      if (ev.denied) bits.push("denied");
      return bits.join(" · ");
    }
    if (e === "warm_open" || e === "warm_close") {
      const bits = [];
      if (ev.reason) bits.push(ev.reason);
      if (ev.open_ms != null) bits.push("open " + ev.open_ms + "ms");
      if (ev.requests != null) bits.push(ev.requests + " reqs");
      if (ev.idle_secs != null) bits.push("idle " + ev.idle_secs + "s");
      return bits.join(" · ") || "—";
    }
    if (String(e).startsWith("admin_")) {
      const bits = [e.replace(/^admin_/, "")];
      if (ev.by) bits.push("by " + ev.by);
      if (ev.corpus_id) bits.push(ev.corpus_id);
      if (ev.ok === false) bits.push("failed");
      return bits.join(" · ");
    }
    if (e === "start") {
      return "pid " + (ev.pid != null ? ev.pid : "—") + " · v" + (ev.version || "?");
    }
    if (e === "warm_state") {
      return "warm=" + (ev.warm != null ? ev.warm : "?") + (ev.rss_bytes != null ? " · rss " + ev.rss_bytes : "");
    }
    try {
      return JSON.stringify(ev);
    } catch (_) {
      return "—";
    }
  }

  function renderActivity(data) {
    if (!data.enabled) {
      $("activity-view").innerHTML =
        '<p class="list-empty"><strong>Activity log off</strong>' +
        esc(data.hint || "Start FQS with --activity-log / FQS_ACTIVITY_LOG.") +
        "</p>";
      $("activity-out").textContent = JSON.stringify(data, null, 2);
      return;
    }
    if (data.ok === false) {
      $("activity-view").innerHTML =
        '<p class="list-empty"><strong>Could not read log</strong>' + esc(data.error || "unknown") + "</p>";
      $("activity-out").textContent = JSON.stringify(data, null, 2);
      return;
    }

    const sum = data.summary || {};
    const q = sum.queries || {};
    const warm = sum.warm || {};
    const win = data.window || {};
    const cfg = data.config || {};

    let html = '<div class="stat-grid">';
    html += stat("Log", esc((data.path || "").split("/").pop() || "—"), "ok");
    html += stat("Queries (window)", String(q.total || 0), q.error ? "warn" : "");
    html += stat("OK / error", (q.ok || 0) + " / " + (q.error || 0), q.error ? "bad" : "ok");
    html += stat("Avg elapsed", q.avg_elapsed_ms != null ? q.avg_elapsed_ms + " ms" : "—", "muted");
    html += stat("Warm open / close", (warm.opens || 0) + " / " + (warm.closes || 0), "");
    html += stat("Admin writes", String(sum.admin_writes || 0), "");
    html += "</div>";

    html +=
      '<p class="muted muted-sm">Users: ' +
      esc(cfg.users || "—") +
      " · scanned " +
      esc(win.bytes_scanned != null ? win.bytes_scanned : "—") +
      " of " +
      esc(win.file_bytes != null ? win.file_bytes : "—") +
      " bytes" +
      (win.truncated_read ? " (partial file)" : "") +
      " · " +
      esc(data.path || "") +
      "</p>";

    if (sum.last_start) {
      html +=
        '<p class="muted muted-sm">Last start in window: ' +
        esc(sum.last_start.ts || "—") +
        " · pid " +
        esc(sum.last_start.pid != null ? sum.last_start.pid : "—") +
        " · v" +
        esc(sum.last_start.version || "?") +
        "</p>";
    }

    const top = sum.top_corpora || [];
    if (top.length) {
      html += '<h3 class="section-title">Top corpora (queries in window)</h3>';
      html += '<div class="chip-row chip-row-spaced">';
      top.forEach((c) => {
        html +=
          '<span class="chip">' +
          esc(c.corpus) +
          " · " +
          esc(c.queries) +
          "</span>";
      });
      html += "</div>";
    }

    const byEv = sum.by_event || [];
    if (byEv.length) {
      html += '<h3 class="section-title">Events in window</h3>';
      html += '<div class="chip-row chip-row-spaced">';
      byEv.forEach((row) => {
        html +=
          '<span class="chip">' +
          esc(row.event) +
          " · " +
          esc(row.count) +
          "</span>";
      });
      html += "</div>";
    }

    const events = data.events || [];
    html += '<h3 class="section-title">Recent' + (data.truncated ? " (truncated)" : "") + "</h3>";
    if (!events.length) {
      html += '<p class="muted">No matching events in the scanned window.</p>';
    } else {
      html +=
        '<table class="data-table"><thead><tr>' +
        "<th>When</th><th>Event</th><th>Corpus</th><th>Who</th><th>Detail</th>" +
        "</tr></thead><tbody>";
      events.forEach((ev) => {
        const who = ev.user || ev.by || ev.role || "—";
        const corpus = ev.corpus || ev.corpus_id || "—";
        const status =
          ev.status != null
            ? statusPill(ev.status >= 400 ? "failed" : ev.status >= 200 ? "ok" : String(ev.status))
            : ev.event && String(ev.event).startsWith("admin_")
              ? statusPill(ev.ok === false ? "failed" : "ok")
              : "";
        html +=
          "<tr>" +
          "<td class='mono muted-xs'>" +
          esc(ev.ts || "—") +
          "</td>" +
          "<td>" +
          statusPill(ev.event || "—") +
          status +
          "</td>" +
          "<td class='mono'>" +
          esc(corpus) +
          "</td>" +
          "<td class='mono muted-xs'>" +
          esc(who) +
          "</td>" +
          "<td class='muted-sm'>" +
          esc(activityDetail(ev)) +
          "</td>" +
          "</tr>";
      });
      html += "</tbody></table>";
    }

    $("activity-view").innerHTML = html;
    $("activity-out").textContent = JSON.stringify(data, null, 2);
  }

  async function refreshActivity() {
    if (!token()) {
      $("activity-view").innerHTML =
        '<p class="list-empty"><strong>JWT required</strong>Activity loads only after you enter an admin token.</p>';
      $("activity-out").textContent = "—";
      return;
    }
    const event = ($("activity-event") && $("activity-event").value) || "interesting";
    const corpus = ($("activity-corpus") && $("activity-corpus").value.trim()) || "";
    const limit = ($("activity-limit") && $("activity-limit").value) || "100";
    let path = "/activity?limit=" + encodeURIComponent(limit) + "&event=" + encodeURIComponent(event);
    if (corpus) path += "&corpus=" + encodeURIComponent(corpus);
    const data = await api(path);
    renderActivity(data);
  }

  function setTab(name) {
    document.querySelectorAll(".tab").forEach((b) => {
      b.classList.toggle("active", b.dataset.tab === name);
    });
    ["edit", "scan", "backends", "frontends", "settings", "activity", "health", "reindex"].forEach((t) => {
      const el = $("tab-" + t);
      if (el) el.hidden = t !== name;
    });
    if (name === "health") refreshHealth().catch((e) => showBanner(e.message, true));
    if (name === "settings") refreshSettings().catch((e) => showBanner(e.message, true));
    if (name === "activity") refreshActivity().catch((e) => showBanner(e.message, true));
    if (name === "reindex") refreshReindex().catch((e) => showBanner(e.message, true));
    if (name === "backends") refreshBackends().catch((e) => showBanner(e.message, true));
    if (name === "frontends") refreshFrontends().catch((e) => showBanner(e.message, true));
  }

  function renderBackends(data) {
    const rows = data.backends || [];
    let html = '<div class="stat-grid">';
    html += stat("Backends", String(rows.length), "");
    html += stat("Installed", String(rows.filter((b) => b.installed).length), "ok");
    html += "</div>";
    html +=
      '<table class="data-table"><thead><tr>' +
      "<th>Status</th><th>Backend</th><th>Role</th><th>Version / path</th><th>Notes</th>" +
      "</tr></thead><tbody>";
    rows.forEach((b) => {
      const ok = !!b.installed && b.healthy !== false;
      html +=
        "<tr>" +
        "<td>" +
        statusPill(ok ? "ok" : "missing") +
        "</td>" +
        "<td><strong>" +
        esc(b.label || b.id) +
        '</strong><div class="mono muted muted-xs">' +
        esc(b.id) +
        "</div></td>" +
        "<td>" +
        esc(b.role || "—") +
        "</td>" +
        "<td class='mono'>" +
        esc(b.version || "—") +
        (b.path ? "<div class='muted muted-xs'>" + esc(b.path) + "</div>" : "") +
        "</td>" +
        "<td class='muted'>" +
        esc((b.notes || []).join("; ")) +
        "</td>" +
        "</tr>";
    });
    html += "</tbody></table>";
    $("backends-view").innerHTML = html;
    $("backends-out").textContent = JSON.stringify(data, null, 2);
  }

  function renderFrontends(data) {
    const kinds = data.kinds || [];
    const rows = data.frontends || [];
    let html = '<div class="stat-grid">';
    html += stat("Kinds handled", String(kinds.length || 0), "");
    html += stat(
      "In use / configured",
      String(
        kinds.filter((k) =>
          ["healthy", "configured", "present", "in_use", "catalog_only"].includes(k.status)
        ).length
      ),
      "ok"
    );
    html += stat("Instances", String(rows.length), "");
    html += stat(
      "Restartable",
      String(rows.filter((f) => f.restartable).length),
      ""
    );
    html += "</div>";
    if (data.corpora_note) {
      html += '<p class="muted muted-sm85">' + esc(data.corpora_note) + "</p>";
    }
    if (data.restart_policy) {
      html += '<p class="muted muted-sm85">' + esc(data.restart_policy) + "</p>";
    }

    if (!kinds.length) {
      html += '<p class="muted">No frontend kinds returned.</p>';
    } else {
      kinds.forEach((k) => {
        const st = k.status || "not_configured";
        let pillLabel = "unused";
        let pillCls = "warn";
        if (st === "healthy") {
          pillLabel = "ok";
          pillCls = "ok";
        } else if (st === "configured" || st === "in_use" || st === "present") {
          pillLabel = st === "in_use" ? "in use" : "configured";
          pillCls = "warn";
        } else if (st === "catalog_only") {
          pillLabel = "catalog";
          pillCls = "warn";
        } else if (st === "not_configured") {
          // Per-corpus UIs (TEITOK) are not a single install — "unused" ≠ missing software.
          pillLabel = k.centralized === false ? "unused" : "not set";
          pillCls = "";
        }
        html += '<div class="frontend-kind">';
        html +=
          '<div class="frontend-kind-head">' +
          '<span class="pill ' +
          pillCls +
          '">' +
          esc(pillLabel) +
          "</span>" +
          " <strong>" +
          esc(k.label || k.id) +
          '</strong> <span class="muted">' +
          esc(k.centralized ? "centralized" : "per-corpus") +
          " · " +
          esc(st) +
          (k.corpus_count ? " · " + k.corpus_count + " corpora" : "") +
          "</span></div>";
        if (k.notes) {
          html += '<p class="muted muted-sm kind-notes">' + esc(k.notes) + "</p>";
        }
        const instances = k.instances || [];
        if (!instances.length) {
          html +=
            k.centralized === false
              ? '<p class="muted muted-sm85">No catalog corpora linked yet (set <code>project_url</code>, <code>interface_preference=teitok</code>, <code>supports_xml</code>, or a TEITOK <code>project_root</code>). This does not mean TEITOK is missing from the server.</p>'
              : '<p class="muted muted-sm85">Not configured in catalog or <code>fqs.json</code>.</p>';
        } else {
          instances.forEach((f) => {
            const h = f.health || {};
            const healthy = !!h.ok;
            html += '<div class="frontend-instance">';
            html +=
              "<div class='row row-start-wrap'>" +
              statusPill(healthy ? "ok" : h.note ? "ok" : "down") +
              "<div><strong>" +
              esc(f.label || f.id) +
              '</strong><div class="mono muted muted-xs">' +
              esc(f.id) +
              (f.source ? " · " + esc(f.source) : "") +
              "</div></div>";
            if (f.url) {
              html +=
                '<div class="mono"><a href="' +
                esc(f.url) +
                '" target="_blank" rel="noopener">' +
                esc(f.url) +
                "</a></div>";
            }
            if (f.restartable) {
              html +=
                "<button type='button' class='secondary btn-fe-restart' data-id='" +
                esc(f.id) +
                "'>Restart</button>";
            }
            html += "</div>";
            if (h.error) {
              html +=
                '<div class="err err-xs">' +
                esc(h.error) +
                "</div>";
            } else if (h.note) {
              html +=
                '<div class="muted muted-xs-mt">' +
                esc(h.note) +
                "</div>";
            }
            html += '<div class="mt-sm"><span class="muted label-sm">Corpora served</span>' +
              corporaServedCell(f) +
              "</div>";
            html += "</div>";
          });
        }
        // Kind-level corpora when no instance nested them (summary)
        if (
          k.centralized &&
          (!instances.length || instances.every((i) => !(i.corpora || []).length)) &&
          (k.corpora || []).length
        ) {
          html +=
            '<div class="mt-sm"><span class="muted label-sm">Corpora (catalog)</span>' +
            corporaServedCell({ centralized: true, corpora: k.corpora }) +
            "</div>";
        }
        html += "</div>";
      });
    }

    if (data.gunicorn_processes && data.gunicorn_processes.length) {
      html += '<h3 class="section-title">gunicorn processes (informational)</h3>';
      html +=
        '<table class="data-table"><thead><tr><th>PID</th><th>Command</th></tr></thead><tbody>';
      data.gunicorn_processes.forEach((g) => {
        html +=
          "<tr><td class='mono'>" +
          esc(g.pid) +
          "</td><td class='mono'>" +
          esc(g.cmd) +
          "</td></tr>";
      });
      html += "</tbody></table>";
    }
    $("frontends-view").innerHTML = html;
    $("frontends-out").textContent = JSON.stringify(data, null, 2);
    $("frontends-view").querySelectorAll(".btn-fe-restart").forEach((btn) => {
      btn.addEventListener("click", () => restartFrontend(btn.dataset.id));
    });
  }

  function corporaServedCell(f) {
    const corps = f.corpora || [];
    if (!corps.length) {
      return '<p class="muted corpus-none">none in FQS catalog yet</p>';
    }
    const aliases = f.corpus_aliases || {};
    let html = '<ul class="corpus-served">';
    corps.forEach((id) => {
      const alias = aliases[id];
      html +=
        '<li class="mono">' +
        esc(id) +
        (alias && alias !== id
          ? ' <span class="muted">(' + esc(alias) + ")</span>"
          : "") +
        "</li>";
    });
    html += "</ul>";
    return html;
  }

  async function refreshBackends() {
    if (!token()) {
      $("backends-view").innerHTML =
        '<p class="list-empty"><strong>JWT required</strong></p>';
      return;
    }
    renderBackends(await api("/backends"));
  }

  async function refreshFrontends() {
    if (!token()) {
      $("frontends-view").innerHTML =
        '<p class="list-empty"><strong>JWT required</strong></p>';
      return;
    }
    renderFrontends(await api("/frontends"));
  }

  async function restartFrontend(id) {
    try {
      if (!confirm("Restart frontend '" + id + "' via its configured restart action?")) return;
      const data = await api("/frontends/" + encodeURIComponent(id) + "/restart", {
        method: "POST",
        body: "{}",
      });
      showBanner(
        data.ok ? "Restart OK for " + id : "Restart reported failure for " + id,
        !data.ok
      );
      await refreshFrontends();
    } catch (e) {
      showBanner(e.message, true);
    }
  }

  let lastScan = null;

  function renderScan(data) {
    lastScan = data;
    const sum = data.summary || {};
    let html = '<div class="stat-grid">';
    html += stat("Roots used", String(sum.roots || 0), "");
    html += stat("Candidates", String(sum.candidates || 0), "");
    html += stat("New", String(sum.new || 0), sum.new ? "ok" : "muted");
    html += stat("Aliases", String(sum.alias || 0), sum.alias ? "" : "muted");
    html += stat("Already registered", String(sum.registered || 0), "muted");
    if (sum.truncated) html += stat("Truncated", "yes", "bad");
    html += "</div>";

    if (data.roots && data.roots.length) {
      html += '<p class="muted muted-sm">Roots: ' +
        data.roots.map((r) => esc(r.path) + " <em>(" + esc(r.source) + ")</em>").join(" · ") +
        "</p>";
    }

    const rows = data.candidates || [];
    if (!rows.length) {
      html += '<p class="muted">No corpus-like directories found under the scan roots.</p>';
    } else {
      html +=
        '<table class="data-table"><thead><tr>' +
        "<th>Status</th><th>Kind</th><th>Suggested id</th><th>Path</th><th>Match</th><th></th>" +
        "</tr></thead><tbody>";
      rows.forEach((c, i) => {
        const st = c.status || "new";
        let pillCls = st === "new" ? "ok" : st === "alias" ? "warn" : "";
        html +=
          "<tr>" +
          "<td><span class='pill " +
          pillCls +
          "'>" +
          esc(st) +
          "</span></td>" +
          "<td>" +
          esc(c.kind) +
          "</td>" +
          "<td class='mono'>" +
          esc(c.suggested_id) +
          "</td>" +
          "<td class='mono'>" +
          esc(c.path) +
          (c.notes && c.notes.length
            ? "<div class='muted muted-xs'>" + esc(c.notes.join("; ")) + "</div>"
            : "") +
          "</td>" +
          "<td>" +
          (c.matched_corpus_id
            ? "<span class='mono'>" +
              esc(c.matched_corpus_id) +
              "</span><div class='muted muted-xs'>" +
              esc(c.match_reason || "") +
              "</div>"
            : "—") +
          "</td>" +
          "<td>" +
          (st === "new"
            ? "<button type='button' class='secondary btn-register' data-i='" +
              i +
              "'>Register</button>"
            : "") +
          "</td>" +
          "</tr>";
      });
      html += "</tbody></table>";
    }
    $("scan-view").innerHTML = html;
    $("scan-out").textContent = JSON.stringify(data, null, 2);
    $("scan-view").querySelectorAll(".btn-register").forEach((btn) => {
      btn.addEventListener("click", () => registerCandidate(Number(btn.dataset.i)));
    });
  }

  async function registerCandidate(i) {
    try {
      const c = (lastScan && lastScan.candidates && lastScan.candidates[i]) || null;
      if (!c) throw new Error("missing candidate");
      const entry =
        (lastScan.register_suggestions &&
          lastScan.register_suggestions.find((e) => e.id === c.suggested_id)) ||
        null;
      const body = entry || {
        id: c.suggested_id,
        label: c.label,
        project_root: c.project_root,
        preferred_backend: c.preferred_backend,
        source_kind: c.source_kind,
        settings: c.settings || {},
        http_policy_mode: "public_query",
        http_allowed_operations: ["query", "catalog"],
        interfaces: ["query"],
        is_current: true,
      };
      await api("/corpora", { method: "PUT", body: JSON.stringify(body) });
      showBanner("Registered " + body.id, false);
      await refreshCorpora();
      await runScan();
    } catch (e) {
      showBanner(e.message, true);
    }
  }

  async function runScan() {
    if (!token()) {
      $("scan-view").innerHTML =
        '<p class="list-empty"><strong>JWT required</strong>Scan runs only after you enter an admin token.</p>';
      return;
    }
    const extra = ($("scan-roots").value || "").trim();
    const body = {};
    if (extra) {
      body.roots = extra.split(/[:;]/).map((s) => s.trim()).filter(Boolean);
    }
    const data = await api("/scan", { method: "POST", body: JSON.stringify(body) });
    renderScan(data);
  }

  $("btn-save-token").addEventListener("click", () => {
    saveToken();
  });
  $("btn-change-token").addEventListener("click", () => {
    setAuthUi(false);
    tokenEl.focus();
    tokenEl.select();
  });
  tokenEl.addEventListener("keydown", (e) => {
    if (e.key === "Enter") saveToken();
  });
  $("btn-health-refresh").addEventListener("click", () => {
    refreshHealth().catch((e) => showBanner(e.message, true));
  });
  $("btn-fqs-restart").addEventListener("click", async () => {
    try {
      if (
        !confirm(
          "Restart this FQS process via its configured fqs.restart action in fqs.json? In-flight queries will be interrupted."
        )
      ) {
        return;
      }
      const data = await api("/self/restart", { method: "POST", body: "{}" });
      showBanner(
        data.ok ? "FQS restart requested" : "FQS restart reported failure",
        !data.ok
      );
    } catch (e) {
      showBanner(e.message, true);
    }
  });
  $("btn-backends-refresh").addEventListener("click", () => {
    refreshBackends().catch((e) => showBanner(e.message, true));
  });
  $("btn-frontends-refresh").addEventListener("click", () => {
    refreshFrontends().catch((e) => showBanner(e.message, true));
  });
  $("btn-settings-refresh").addEventListener("click", () => {
    refreshSettings().catch((e) => showBanner(e.message, true));
  });
  $("btn-activity-refresh").addEventListener("click", () => {
    refreshActivity().catch((e) => showBanner(e.message, true));
  });
  $("activity-event").addEventListener("change", () => {
    refreshActivity().catch((e) => showBanner(e.message, true));
  });
  $("activity-limit").addEventListener("change", () => {
    refreshActivity().catch((e) => showBanner(e.message, true));
  });
  $("activity-corpus").addEventListener("keydown", (e) => {
    if (e.key === "Enter") refreshActivity().catch((err) => showBanner(err.message, true));
  });
  $("btn-scan").addEventListener("click", () => {
    runScan().catch((e) => showBanner(e.message, true));
  });
  $("btn-reindex-refresh").addEventListener("click", () => {
    refreshReindex().catch((e) => showBanner(e.message, true));
  });
  $("reindex-status").addEventListener("change", () => {
    refreshReindex().catch((e) => showBanner(e.message, true));
  });
  $("btn-refresh").addEventListener("click", () => {
    if (!token()) {
      showBanner("Enter an admin JWT first — the catalog is not loaded without it.", true);
      renderList();
      return;
    }
    refreshCorpora()
      .then(() => setTab(document.querySelector(".tab.active").dataset.tab))
      .catch((e) => showBanner(e.message, true));
  });
  if (filterEl) filterEl.addEventListener("input", renderList);
  $("labels-pick").addEventListener("change", () => {
    const v = $("labels-pick").value;
    if (!v) return;
    addExistingLabel(v);
  });
  $("labels-add-new-btn").addEventListener("click", () => {
    const row = $("labels-new-row");
    if (!row) return;
    row.hidden = false;
    const inp = $("labels-new-input");
    if (inp) {
      inp.focus();
      inp.select();
    }
  });
  $("labels-new-cancel").addEventListener("click", () => {
    $("labels-new-row").hidden = true;
    $("labels-new-input").value = "";
  });
  $("labels-new-confirm").addEventListener("click", () => {
    try {
      addNewLabel($("labels-new-input").value);
      showBanner("", false);
    } catch (e) {
      showBanner(e.message, true);
    }
  });
  $("labels-new-input").addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      $("labels-new-confirm").click();
    } else if (e.key === "Escape") {
      $("labels-new-cancel").click();
    }
  });

  $("btn-new").addEventListener("click", () => {
    if (!token()) {
      showBanner("Enter an admin JWT before creating a corpus.", true);
      renderList();
      return;
    }
    draftNew = true;
    selectedId = null;
    fillForm(emptyEntry());
    renderList();
    setTab("edit");
  });
  $("btn-save").addEventListener("click", async () => {
    try {
      const entry = readForm();
      await api("/corpora", { method: "PUT", body: JSON.stringify(entry) });
      draftNew = false;
      selectedId = entry.id;
      showBanner("Saved " + entry.id, false);
      await refreshCorpora();
    } catch (e) {
      showBanner(e.message, true);
    }
  });
  async function runValidate(full) {
    try {
      const id = $("f-id").value.trim();
      if (!id) throw new Error("save or select a corpus first");
      const data = await api("/corpora/" + encodeURIComponent(id) + "/validate", {
        method: "POST",
        body: JSON.stringify({ full: !!full, strict_full: false }),
      });
      $("validate-out").hidden = false;
      $("validate-out").textContent = JSON.stringify(data.result || data, null, 2);
      showBanner(data.result && data.result.ok ? "Validate ok" : "Validate reported problems", !(data.result && data.result.ok));
      await refreshCorpora();
    } catch (e) {
      showBanner(e.message, true);
    }
  }
  $("btn-validate").addEventListener("click", () => runValidate(false));
  $("btn-validate-full").addEventListener("click", () => runValidate(true));
  $("btn-delete").addEventListener("click", async () => {
    try {
      const id = $("f-id").value.trim();
      if (!id) throw new Error("no corpus id");
      if (
        !confirm(
          "Deactivate corpus '" +
            id +
            "' (supersede / hide from default listings)? Files on disk are not touched. Permanent catalog removal is CLI-only: fqs corpora delete --id … --force"
        )
      ) {
        return;
      }
      await api("/corpora/" + encodeURIComponent(id) + "?supersede=1", { method: "DELETE" });
      selectedId = null;
      showBanner("Deactivated " + id, false);
      await refreshCorpora();
    } catch (e) {
      showBanner(e.message, true);
    }
  });
  document.querySelectorAll(".tab").forEach((b) => {
    b.addEventListener("click", () => setTab(b.dataset.tab));
  });

  try {
    localStorage.removeItem(TOKEN_KEY);
  } catch (_) {}
  tokenEl.value = sessionStorage.getItem(TOKEN_KEY) || "";
  renderList();
  if (!token()) {
    setAuthUi(false);
    showBanner("Paste an admin JWT from fqs admin-token, then Use token.", false);
  } else if (!looksLikeJwt(token())) {
    setAuthUi(false);
    showBanner(
      "Stored value does not look like a JWT. Clear it and run: fqs admin-token",
      true
    );
  } else {
    refreshCorpora()
      .then(() => {
        setAuthUi(true);
        showBanner("", false);
      })
      .catch((e) => {
        setAuthUi(false);
        showBanner(e.message, true);
      });
  }
})();
