# Psychiatry Inpatient Document Audit System

> The head nurse drops the day's batch of inpatient document bundles into the window; a dozen seconds later every record lights up with "which admission is missing which documents, which document has an out-of-order timeline, which rule codes are violated, and what the basis is". Fillable gaps can be previewed, confirmed on the spot, written back as a new resource with a provenance trail, and the medical records department checks off the same list to reach "archivable".

Documents in a psychiatric ward are a **set**: informed consent for restraint, restraint records, restraint release records, assessment reports, signature pages. Missing one, one with an inverted timeline, one missing a signatory — the record cannot be archived. Yet the completeness of this "controlled document set" is usually verified by human eyes flipping across a dozen screens. This system turns that reconciliation seam into **one machine, two views**: a resident tray process watches for record changes and re-runs only the changed documents; the head nurse uses "Document Package Audit" to get a list with paragraph-level locations, while the records department uses "Archive Reconciliation" to see the forward and reverse dependency graph plus days-overdue counts. The system only performs **document-level** missing-item, timeline and completeness checks plus resource-creation write-back. It produces no diagnosis or treatment advice, and performs no electronic signature or proxy signing.

## Use Cases / Target Roles

| Role | When | What you get |
|------|------------|----------|
| Psychiatric ward head nurse | Before morning handover, after a night event (aggression / self-harm / restraint) | A missing-item and timeline list aggregated per admission, each entry carrying a rule code, paragraph location and basis; fillable items can be previewed and confirmed on the spot |
| Ward documentation specialist | Submitting the document package before discharge | Pre-submission pre-check: which document is missing, which timeline is wrong; fix them before submitting instead of leaving gaps for the records department |
| Medical records QC reviewer | Daily reconciliation and archiving of discharged records | Archive reconciliation view: who is missing what, what this blocks, how many days overdue; one-click dispatch of missing-document to-dos |
| Quality control office | Monthly spot checks / retrospective review | Replay by the rule set version in effect at the time: which rule version audited a given record, and which codes it hit |

## Key Capabilities

- **One drag-in, a whole batch of results**: a whole folder is aggregated by admission alias into an audit queue; within seconds each entry reports "rule code + which document, which item, which paragraph + which specification clause it derives from", so there is no need to page through documents one by one.
- **Locatable down to the paragraph, not just "there is a problem"**: every finding carries the document alias, item ordinal, paragraph ordinal and in-paragraph character range, with the offending paragraph fully highlighted; cross-document timeline problems automatically present the locations of both items side by side.
- **No one-click fix for what cannot be auto-fixed**: entries requiring a clinician's own judgement show only the violated basis and offer **no** "fix it in one click", so documentation responsibility is never pushed onto the tool.
- **Write-back always requires per-item human tick**: filled-in content first lands in a local draft, is compared against the original in two columns, and only after confirmation is sent as a single resource-creation request that generates an audit trail; sending is refused outright if nothing is ticked.
- **The same document is never created twice in a day, and can be re-checked later against the version in effect**: write-back carries an idempotency key, so a repeated confirmation only reports "already submitted by <person> at <time>" and produces no second record; every audit persists the rule set version together with the hit codes, so the QC office can restore "which rule version audited this record back then".

## Quick Start

### Dependencies

| Dependency | Version | Notes |
|------|------|------|
| Python | ≥ 3.10 | Desktop shell runtime |
| Rust | ≥ 1.75 | Toolchain for building the core crate |
| PySide6 / pyo3 | ≥ 6.6 / ≥ 0.22 | GUI widgets and QSS styling / binding between the Rust core and the desktop shell |
| maturin (optional) | ≥ 1.4 | Packaging the bindings; `run.sh` falls back to `cargo build` when it is absent |
| Network (write-back and notifications only) | Reachable in-hospital FHIR R4 endpoint | Not needed for local auditing; missing credentials do not block startup |

### Installation

```bash
git clone <repo-url> psych-inpatient-doc-audit && cd psych-inpatient-doc-audit
python3 -m venv .venv && source .venv/bin/activate && pip install -e .

# Build the Rust core (cdylib + CLI binary): uses maturin if installed, otherwise plain cargo build
maturin develop --manifest-path core/crates/psychdoc-ffi/Cargo.toml
# Or (without maturin): ( cd core && cargo build -p psychdoc-ffi --features extension-module )
```

> `run.sh` auto-detects: if the bindings are missing it tries `maturin develop` first, otherwise it uses `cargo build` and installs the artifacts into `bindings/`.

### Configuration

Copy `.env.example` to `.env` and fill in real in-hospital values. The five items required for write-back and missing-document notification (`EMR_*` and `WECOM_*`) are **all listed at once** by the startup self-check when missing — local document package auditing still runs normally, and only the links that genuinely reach the endpoint get `E_CFG_MISSING`; a **missing or invalid rule set refuses startup outright** (exit code 2) — reporting "everything passed" when there are no rules is more dangerous than giving no verdict at all. Neither case **silently degrades** to a local stand-in.
`psychdoc preflight` shows all missing items at once, with zero network requests throughout (see Section 5 of [`docs/examples.md`](docs/examples.md) for the real output).

| Config item | Default | Notes |
|--------|--------|------|
| `EMR_BASE_URL` | none (required) | Root address of the in-hospital FHIR R4 service |
| `EMR_CLIENT_ID` | none (required) | OAuth2 client identifier |
| `EMR_CLIENT_SECRET` | none (required) | OAuth2 client secret |
| `EMR_TENANT_ID` | none (required) | In-hospital tenant identifier, sent via the `X-Tenant-Id` header |
| `WECOM_WEBHOOK_URL` | none (required) | WeCom group robot address, used for missing-document notifications |
| `PSYCHDOC_RULESET` / `PSYCHDOC_CACHE` | `rulesets/hospital.yaml` / none (incremental mode off by default) | Rule set path (relative paths resolve against the working directory at startup); incremental cache file path — when configured, unchanged documents are skipped by content hash |
| `PSYCHDOC_DATA_DIR` / `PSYCHDOC_LOG_LEVEL` | Platform user data directory / `info` | Where sqlite is stored (never inside the project directory); structured log level, an unrecognized level raises `E_CFG_INVALID` rather than falling back to the default |

### Build and Verify

```bash
( cd core && cargo test )             # Core crate unit tests (parsing / rules / dependency graph / storage / integration layer / CLI contract)
python -m pytest tests -q             # Desktop shell and integration tests
./run.sh                              # Launch the desktop window + resident tray process
```

### Main Command Quick Reference

```bash
./run.sh                                       # Launch the desktop window + resident tray process
psychdoc preflight                             # Startup self-check: lists all missing items at once, 0 network requests
psychdoc check fixtures/ --ruleset rulesets/hospital.yaml              # Run one audit without the GUI
psychdoc check fixtures/ --ruleset rulesets/hospital.yaml --cache /tmp/pc.json   # Incremental: re-run only the changed documents
psychdoc graph WARD-B-001 --input fixtures/ --ruleset rulesets/hospital.yaml --direction both
psychdoc writeback-dryrun selection.json       # Build the resource-creation request summary only, without sending
psychdoc rule-regress / export-audit / codes   # Rule regression / audit package export / look up error codes
```

## Commands / API / Configuration

### Key Configuration Items

See the "Configuration" table above. Credentials are always read from environment variables, never hard-coded, and never written into sqlite, run logs or exported packages; of the seven CLI subcommands **only `check --live` needs credentials** — the other six are purely local and can be run in full on a machine with no in-hospital connection.

### CLI Subcommands / Interface Endpoints

The no-GUI regression and operations channel (exit code contract 0/1/2/3):

| Command or path | Purpose | Key arguments |
|------------|------|----------|
| `psychdoc check` | Run one audit over a single Bundle or a whole directory, output the findings list | `<bundle\|dir>`, `--ruleset`, `--cache`, `--record`, `--actor`, `--live` |
| `psychdoc graph` | Output archive reconciliation view data (forward and reverse) | `<admission alias>`, `--input`, `--ruleset`, `--direction`, `--as-of` |
| `psychdoc writeback-dryrun` | Build the resource-creation request and print a summary, **sending no network requests at all** | `<selection.json>`, `--out <Bundle.json>` |
| `psychdoc rule-regress` | Run a batch of rule regression cases and output per-code coverage | `--cases`, `--ruleset`, `--root`, `--only`, `--json` |
| `psychdoc export-audit` | Export an audit package (rule set version + hit codes + actor + timestamp) | `--db`, `--alias`, `--run`, `--since`, `--until`, `--at`, `--format`, `--out` |
| `psychdoc codes` / `psychdoc preflight` | Export the full error code table, or look up handling advice for one code; the startup self-check lists all missing items at once (zero network requests) | `--format markdown\|json\|text`, `--code` |
| `GET {EMR_BASE_URL}/fhir/r4/Composition` | Read document items, supports `_since` incremental fetch and Bundle pagination | `Authorization`, `X-Tenant-Id`, `Accept` |
| `POST {EMR_BASE_URL}/fhir/r4` | Send a Bundle transaction to create resources and generate a Provenance; `POST {WECOM_WEBHOOK_URL}` sends missing-document notifications | `Authorization`, `X-Tenant-Id`, `msgtype=markdown` |

Global options: `--json` makes stdout emit a single machine-readable JSON document (human-readable process text goes to the log), and `--log-file <path>` writes the full process to a log file on disk. See [`docs/cli.md`](docs/cli.md) for all arguments and field specifications.

### Errors and Degradation

| Scenario | Behaviour | Handling |
|------|----------|----------|
| Credentials missing / empty | `E_CFG_MISSING` raised at startup, listing every missing item, exit code 2 | Fill in the environment variables and restart; it **does not** degrade to offline mode |
| Rule set references an undefined document type / duplicate codes | `E_RULESET_INVALID`, that batch **refuses to start** | Fix the rule set and re-run; never treated as "no rules = passed" |
| Documents ambiguous when paired by name | That item is marked `ambiguous` and joins the manual queue | A human resolves the ambiguity on the EMR side and reloads; no silent pairing |
| Timestamps missing or of insufficient precision | That timeline rule is skipped and `skipped_reason` is recorded | Supply the business timestamp inside the document; never back-filled with the current time |
| Document dependency specification forms a cycle | `E_DEP_CYCLE`, nodes inside the cycle are marked `ambiguous`, no edges are produced at all | Fix the in-hospital dependency specification; nodes outside the cycle are still drawn normally |
| Resource-creation request fails midway / endpoint rate-limits | The whole batch rolls back with **0 partial writes**, the local draft is kept; reads back off per `Retry-After` | Fix the endpoint and resend from the draft, without duplicating the part that already took effect |
| Same document confirmed again the same day | Idempotency key hits, returning the actor and timestamp of the first submission, **0 new write-backs** | Nothing to do; if a resubmission is genuinely needed, change the content first to produce a new key |
| A rule code lacks positive / negative / boundary cases | The coverage matrix is marked "gap", coverage falls below 100%, exit code 1 | Add all three kinds of cases; deleting cases does not count as a regression pass |
| Endpoint rate-limiting / transient failure | Reads retry per `Retry-After` or exponential backoff; writes retry at most 3 times then report an explicit error code | Retry later; queued notifications are re-sent |

**The full error code table** (trigger condition, retryability and handling advice for each entry) is available straight from the command, no need to read source. The table is generated by the code:
if a new code is added in code but not registered in the table, `cargo test -p psychdoc-store --test observability` fails on the spot and points out the missing code.
Every audit also leaves a structured run log under `logs/` in the user data directory (rotated daily, one event per line, directly readable with `jq`).
See also [`docs/error-codes.md`](docs/error-codes.md) for the full table.

```bash
psychdoc codes --code E_SCHEMA_TOO_NEW     # If not found it says so plainly, never fudges with a near match
psychdoc codes --format markdown           # Export the whole table, ready to paste into a ticket or assignment note
```

### Integrating with Existing Systems

- **Read**: `Composition` / `DocumentReference` over FHIR R4 REST, supporting `_since` incremental pulls and Bundle pagination traversal. **Write**: `Bundle` transaction for resource creation plus a `Provenance` audit trail, with the whole batch committed in one transaction; `msgtype=markdown` is additionally sent to a WeCom group robot.
- **Credential sources**: all five credentials are read from environment variables and never hard-coded. The synthetic de-identified Bundles under `fixtures/` belong to the test data layer only — **the stand-in is not the final integration surface**.

## Typical Scenarios

### Scenario 1: Head nurse before morning handover after a night event

Enter "Document Package Audit" from the tray icon and drag in the entire folder exported for the shift; the window aggregates the queue by admission alias, with a missing-count badge in the left column.

Select an admission and the right column lists, entry by entry, what is missing, which timeline is inverted, which rule code is violated and which clause it derives from; for fillable entries click "Preview Fill-in", review the two-column comparison, tick items one by one, and confirm to write back and create the resource.

```bash
psychdoc check fixtures/ --ruleset rulesets/hospital.yaml   # The equivalent no-GUI channel
```

### Scenario 2: Daily archive reconciliation in the records department

Switch to the "Archive Reconciliation" tab and select the admissions pending archiving that day; the dependency graph shows missing documents (red), items blocked transitively (orange) and complete items (green), with blocking edges in bold.
On the right, view days overdue and switch between "what this blocks" and "what this is waiting for"; click "Dispatch missing-document to-do" to push to the department group, after which the button is greyed out and shows the send time and actor.

```bash
psychdoc graph WARD-B-001 --input fixtures/ --ruleset rulesets/hospital.yaml --direction dependencies
psychdoc graph WARD-B-001 --input fixtures/ --ruleset rulesets/hospital.yaml --direction dependents   # The two lists must differ
```

### Scenario 3: Monthly spot checks and retrospective review

Export an audit package for the sampled range (rule set version + hit codes + actor + timestamp + idempotency key); for disputed records, trace back by admission alias and point in time to the rule set version in effect, check the hit-code set and exemption records under that version, and confirm the spot-check conclusion matches the one reached at the time.

```bash
psychdoc export-audit --format csv --out audit.csv    # Print a batch summary before exporting to verify the scope
psychdoc export-audit --alias WARD-I-001 --at 2026-03-05T10:00:00+08:00 --format json   # Trace the version in effect at a point in time
```

## Output Samples

Everything below is **real run output**, with data from the de-identified synthetic document packages under `fixtures/` (no patient names, no admission numbers); full output for each artifact type is in [`docs/examples.md`](docs/examples.md).

### Findings list (`psychdoc check`, exit code 1)

```text
文书包：../fixtures/ward-11-cross-doc-violations.json    规则集：../rulesets/hospital.yaml（版本 0.6.0 · sha256 2abfac00aa5b）
已审文书 5 件 · 问题清单（7 条）：
  [阻断] QC-PSY-05 · WARD-I-001-RECORD-01#s0@[7, 24) · 不可自动修
        约束记录的约束起始时间（2026-03-04 08:00）早于约束解除记录的解除时间（2026-03-04 07:00），
        解除时序倒挂（依据：《精神科住院护理常规》第 6.1 条：约束解除记录填写的解除时间不得早于
        约束记录填写的约束起始时间，二者倒挂的病历不得直接归档）
        规则来源：QC-PSY-05（院级）
  …（违反的两件各给一条定位，是时间序规则的固定形态；另有 QC-PSY-06/07 各两条、QC-PSY-08 一条）
事件时间线（WARD-I-001，13 条事件）：
  2026-03-04 07:00  业务时刻  WARD-I-001-RELEASE-01   2026-03-04 08:00  业务时刻  …-RECORD-01
```

`#s0@[7, 24)` means "paragraph 0, characters 7 to 24"; slicing that range back into the original text lands exactly on the timestamp literal that was judged. The compliant counter-example package `ward-12-cross-doc-compliant.json` outputs "no rule hits found", exit code 0; when a timestamp is missing the system takes the "cannot be judged" path rather than "passed" — the list is empty but a parallel entry `[未判定] QC-PSY-05 · WARD-D-001 · 对照件不在本次文书包内：…` is listed, and that item is marked in the timeline as `（无时刻）  整件抽不出任何时刻，时间序相关检查已跳过` (no timestamp: no timestamp can be extracted from the whole document, timeline-related checks have been skipped).

### Archive reconciliation dependency graph (`psychdoc graph`, exit code 0)

```text
归档核齐视图：WARD-B-001（核算时刻 2026-10-07T01:21:36+08:00）
结论：已核齐（零缺件 · 零连带阻塞 · 零人工待办）
件清单（5 件）：
  [在库] WARD-B-001-CONSENT-01 · 约束知情同意   在等（正向）：无
      卡住（反向）：WARD-B-001-RECORD-01（约束记录）
  [在库] WARD-B-001-RECORD-01 · 约束记录       在等（正向）：WARD-B-001-CONSENT-01（约束知情同意）
      卡住（反向）：WARD-B-001-RELEASE-01（约束解除记录）
  …
口径说明：
  · 阻塞传播只走 requires: true 的依赖边；缺 0 件，连带阻塞 0 件，最深 0 级
  · 欠件天数按自然日（日历日差）计算，不按 24 小时整除；从未登记发现时刻的缺件天数
    显示为「算不出」，不按今天兜底。
```

Querying an admission alias that does not exist exits with code 2 and reports `E_ENCOUNTER_UNKNOWN` (an empty view could be misread as "fully reconciled") rather than returning an empty view.

### Resource-creation request summary and audit export package

```text
写回干跑：WARD-A-001 · 1 条 · 时刻 2026-03-05T08:20:00+08:00
Bundle 类型：transaction（transaction = 要么整批生效，要么零条生效）
Bundle 条目 2 条（其中末条为 Provenance）：POST Composition · POST Provenance
幂等键（同一份内容今天重复提交会被这几条挡下）：
  WARD-A-001-CONSENT-01 · 84b50317895c76e14940b429705308da774db432379d3055093e7a474af33626
本次未发起任何网络请求（干跑：不取 token、不连端点、不落库）。

psychdoc: E_FHIR_TXN_EMPTY：勾选清单是空的，拒绝构造 transaction（与真发同码 E_FHIR_TXN_EMPTY）
```

`writeback-dryrun` makes **0 network requests**, and an empty selection yields the **same code** as a real send. `export-audit --format csv` carries a UTF-8 BOM (opens cleanly in Excel), runs an additional **second de-identification scan** during export, and prints a batch summary before exporting to verify the scope (batch id / admission alias / rule set version + sha256 / actor / timestamp / how many documents audited / hit-code set / how many rows exported); among the 18 header columns, `豁免原因` (exemption reason) and `折叠去向` (fold destination) make "why was this not reported / why was this merged" explainable inside the export package.

## Concept / Measured Comparison

### Visual Target

See [`docs/concepts/visual-target.md`](docs/concepts/visual-target.md): a **structured visual target card** giving three states — initial state, first useful moment, and mature state (palette: `#1F6FEB` actionable / `#D64545` missing / `#2DA44E` compliant / base `#F6F8FA`, typography and information hierarchy, whitespace density, and the constraints that must be reachable). This environment has no image generation capability, so **no image files are placed** and no concept images are fabricated.

### Measured Evidence

The following four images are **genuinely captured during runs**, produced by `scripts/capture_screenshots.py` using `QScreen.grabWindow()` to capture the window's own rendering (not design mockups, not post-hoc composites); the per-image generation commands and fixture sources are documented in that directory's README.

![Initial state: two tabs + drop area + footer placeholder](docs/screenshots/01-起始态.png)
![First useful moment: admissions in the left column + findings list on the right + offending paragraph highlighted in red + basis line](docs/screenshots/02-问题清单与段落高亮.png)
![Mature state: tray badge + archive reconciliation dependency graph (missing red / transitively blocked orange / complete green) + footer run summary](docs/screenshots/03-归档核齐依赖图.png)
![No-GUI regression channel: real terminal output and real exit code 1 from psychdoc check](docs/screenshots/04-cli-check.png)

> **Honest note**: above is the visual target (design intent, `docs/concepts/`), below is the real measurement (captures from actual runs, `docs/screenshots/`). The gap is stated openly and no claim of complete consistency is made: the target card imagined a **free-layout hierarchical dependency graph**, while the measured implementation is a **fixed left-graph / right-list grid** so that everything reads on one 1280×820 screen, with hierarchy expressed by three colours (red/orange/green) plus a "level N" label and no free dragging; the target card specifies a badge "like `缺 3`", while the implementation renders the actual missing count and shows "cannot be computed" rather than "0 days" when days-overdue is unavailable — a deliberate design decision deviating from the target card; the measured images are rendered by native Qt widgets with no extra wrapping.

## Architecture and Data Flow

```
  De-identified document package / in-hospital FHIR R4 endpoint
            │
            ▼
  ┌──────────────────────────────────────────────────────────┐
  │ Discovery & entity binding → three-layer parsing (partial │  Filter by type dictionary; items bound by
  │ success) → rule set loading                               │  Composition.id (pairing by filename is
  │ Hospital → department → individual three levels + ward     │  forbidden); produces an item → field table
  │ exemption predicates + sha256 version freeze               │  → timestamp set; loading validates strictly
  └───────────────────────────┬──────────────────────────────┘  and never degrades
                              ▼
  ┌───────────────────────────────┬──────────────────────────┐
  │ Rule executors A/B/C: QC-PSY-01~08 │ Document dependency │  Executors: item-field class / cross-document
  │ code + paragraph location + basis  │ graph                │  timeline / signature completeness;
  │ + auto-fixable or not            │ Collector→Resolver→  │  dedup, best-candidate selection and
  │                                  │ Tarjan SCC → forward  │  severity tiering. Graph: cycle detection →
  │                                  │ and reverse           │  blocking closure → days overdue
  └───────────────┬───────────────┴──────────┬───────────────┘
                  ▼                          ▼
  ┌──────────────────────────────────────────────────────────┐
  │ Local sqlite: item-level state machine + idempotency key │  Missing → Draft → Created → Received → Archivable;
  │ + audit chain. Write-back draft → per-item human ticking │  reading and write-back each leave a Provenance
  │ → single whole-batch Bundle transaction                   │
  └───────────────────────────┬──────────────────────────────┘
                              ▼
   Desktop window with two views (Document Package Audit / Archive Reconciliation) · tray badge · WeCom missing-document to-do · audit export package
```

### Key Invariants

- **Idempotency key**: `sha256(admission alias + item id + content hash)`, with a unique constraint on the write-back table; a resubmission with the same key returns the actor and timestamp of the first one, with **0 new write-backs**.
- **States cannot skip steps**: the item-level state machine is `Missing → Draft → Created → Received → Archivable`; illegal transitions (such as `Missing → Archivable`) are rejected and recorded as `E_STATE_JUMP`, never silently accepted.
- **Entity binding does not rely on filenames**: items are bound by `Composition.id` / template identifier; documents with the same name across systems produce `ambiguous` output for human handling, never silent pairing.
- **Failures never silently alter data**: a resource-creation transaction that fails midway rolls back the whole batch (0 partial writes) and the local draft is kept for retry; documents that fail to parse enter an `unresolved` list with a reason code — a whole-batch failure is **forbidden**, and so is silent dropping.
- **Versions are traceable and never overstep**: every audit persists the rule set sha256 together with the hit-code set, so the version in effect can be restored later by admission alias + point in time; the system only performs document-level missing-item, timeline and completeness checks plus resource creation — it produces no diagnosis or treatment advice, and performs no electronic signature or proxy signing.

The first four are each enforced by a test suite (an illegal transition changes no existing state, a same-key submission adds zero new rows, same-named documents across systems create zero edges, nodes outside a cycle are not condemned wholesale by a local error). See [`docs/architecture.md`](docs/architecture.md) for the exact definitions.

## Safety and Compliance Boundaries

- **No clinical decision-making**: produces no diagnosis, treatment plan or medication advice; all output is document-level missing-item, timeline and completeness prompting.
- **No electronic signature / proxy signing**: for signature-related problems it only reports "signature completeness gap" and the basis; the signing action itself is still performed on the EMR side by the clinician in person.
- **No patient-identifiable detail is persisted**: only `Composition.id` and its de-identified alias are processed; names / admission numbers / identity documents are neither received nor stored; a second de-identification scan is applied to logs before writing to disk.
- **Write-back requires human confirmation; credentials are never persisted**: filled-in content first lands in a local draft and is submitted as a whole batch only after per-item human ticking; sending is refused outright if nothing is ticked; a withdrawal returns the item to the draft area and produces no effective fact. All five credentials are read from environment variables — never hard-coded, never written into sqlite, never appearing in logs or export packages.

## Acceptance and Self-Check

| Acceptance item | Command | Expected result |
|--------|------|----------|
| Core crate unit tests all green | `cd core && cargo test` | Tests across the parsing / rules / dependency graph / storage / integration layer / CLI contract crates all pass |
| Desktop shell and integration tests pass | `python -m pytest tests -q` | FFI bindings, queue, card rendering, archive view, tray and main window tests pass |
| Startup self-check lists all missing items | `psychdoc preflight` | Lists all 5 missing items at once and states "no silent degradation"; **0 network requests**; exit code 2 |
| One command produces a checkable list | `psychdoc check fixtures/ --ruleset rulesets/hospital.yaml` | Prints a list carrying rule codes, `item-alias#sparagraph@[range]`, specification basis and rule source layer; exit code 1 when blocking findings exist |
| Compliant counter-example yields zero hits | `psychdoc check fixtures/ward-12-cross-doc-compliant.json --ruleset rulesets/hospital.yaml` | Outputs "no rule hits found", exit code 0 (false-positive guard) |
| Incremental mode re-runs only changed documents | Same command plus `--cache /tmp/pc.json`, modify one document and re-run | First run "re-ran N / skipped 0"; second run "re-ran 1 / skipped N-1"; the two diagnostic outputs are equal entry by entry |
| Forward and reverse dependency graphs differ | `psychdoc graph WARD-B-001 --input fixtures/ --ruleset rulesets/hospital.yaml --direction both` | Both directions are individually reproducible and their union equals the reachable set; querying a non-existent alias exits 2 rather than returning an empty view |
| Rule regression all green, no coverage gaps | `psychdoc rule-regress` | All 24 cases (8 codes × positive/negative/boundary) pass, coverage 100%, exit code 0 |
| Dry run sends nothing / empty selection rejected | `psychdoc writeback-dryrun <selection.json>` | Prints only the resource-creation request summary + idempotency key, **0 network requests**; `entries: []` reports `E_FHIR_TXN_EMPTY` and exits 2, the same code as a real send |
| Error codes self-serviceable | `psychdoc codes --code E_DEP_CYCLE` | Prints that code's trigger condition, retryability and handling advice; if not found, says so plainly |
| Scale benchmark and screenshots reproducible | `./scripts/bench_batch_scan.sh` · `python3 scripts/capture_screenshots.py` | A batch scan of ≥200 documents completes within budget with zero silent skips that can be checked (results land in `docs/bench/`); the 4 real screenshots are regenerated |

## Project Structure

```
psych-inpatient-doc-audit/
├── README.md / LICENSE / qrcode.jpg   # This file / MIT / follow-us block image
├── pyproject.toml / .env.example      # PySide6 + maturin build config / sample of the five credentials and four settings
├── run.sh / run.bat                   # One-click local launch (Python / bindings / credential self-check); .gitignore excludes build artifacts such as target
├── core/crates/
│   ├── psychdoc-model/                # Item / field / paragraph / position range / diagnostic triple / item-level state machine
│   ├── psychdoc-parse/                # Discovery · filtering · entity binding · three-layer parsing + incremental cache
│   ├── psychdoc-rules/                # Three-level coverage · sha256 freeze + three executor classes + exemption predicates + regression engine
│   ├── psychdoc-graph/                # Collector→Resolver→forward/reverse graph + cycle detection + blocking closure + days overdue
│   ├── psychdoc-fhir/                 # FHIR R4: read + Bundle transaction write + Provenance + notifications
│   ├── psychdoc-store/                # sqlite persistence + idempotency keys + audit chain + versioned migrations
│   ├── psychdoc-ffi/                  # PyO3 binding layer: 8 synchronous entry points (api business + transport + jsonval conversion)
│   └── psychdoc/                      # CLI binary: seven subcommands + 0/1/2/3 exit code contract
├── desktop/                           # PySide6 desktop shell
│   ├── main_window.py / archive_view.py / doc_card.py   # Two-tab main window / archive reconciliation view / document card (highlight + basis line + two-column diff)
│   ├── tray.py / workers.py / theme.py                # Resident tray / QtConcurrent / QSS
│   └── core_bridge.py / main.py                        # Binding location · queue aggregation · error code normalisation / startup self-check
├── conftest.py / bindings/            # pytest environment (offscreen) / locally built artifacts
├── rulesets/ fixtures/                # Rule sets and 24 regression cases / de-identified synthetic document packages
├── templates/                         # Paragraph skeleton notes (the skeletons themselves travel with the rule set — see that directory's README)
├── scripts/ tests/                    # Screenshot capture · fixture generation · batch scan and scale benchmark scripts / desktop shell and integration tests
└── docs/                              # cli.md (all CLI arguments) · architecture.md (state machine · idempotency key · dependency graph · audit chain)
                                        # examples.md (full output) · error-codes.md (complete error code table)
                                        # rule-regress-coverage.md · fixture-catalog.md · e2e-reproduction.md · bench/ · concepts/ · screenshots/
```

## License

MIT

---

## Follow Us

Scan the QR code to follow our official account for updates and community access:

![Follow Us](qrcode.jpg)