# Multi-device bridges — design v7 (Bridge Runtime R2 + Bridge Kit S4)

Status: APPROVED by the owner 2026-10-10 (v7) (v6 + its gate's 3 findings; that gate closed the rest and found no new shape issue). Amended after the R2 build's gate (2026-10-10), as the lead decided: §3 (omitted `via`, Config equality, kind mismatch, disable validation, read-back tag), D4 and §4.1 (driver panic), §4.3 (packet ids, failed state, committed Config, S2 window), §4.4 (frozen retirement; the owner's decision on property Desired while disabled), §4.5 (sweeps, first-replay litter), §4.6, §4.7; and after its confirming reviews (2026-10-10): §3 (a disable during a replacement), §4.3 (every publish budgeted, the shutdown's flush), §4.5 (a cleanup's queued retractions; one cancelled by a renewed Config during the stop), §4.6, §4.7, §4.9. Three design gates (v1, v3, v4) kept finding races in the
per-device isolation machinery (generations, input cache, revocation, cross-process cleanup). The
owner switched D4 to (a): a stuck device worker restarts the process; every other device failure stays
per device. With that, v5 drops generations, the input cache, revocation and the leaked-thread ledger,
and bootstraps each device from its own per-instance subscriptions (a genuine broker replay).
Tier T2 (it mints wire contract).

Amended after the one-runtime merge (2026-10-10, owner decision D9: "There should not be multiple
versions"): D1, D9, §4.1, §4.3, §4.6, the new §4.9 (the fixed instance is the host's N = 1 mode),
§7 and §8. R1's separate session path is deleted; a `Bridge` is the host with one fixed instance.

Amended after the Store's S4a landed (Store main c6360a333, 2026-10-12; `docs/superpowers/specs/2026-10-11-bridge-kit-s4a-design.md`
in the Store repo, whose §19 deviations D1-D45 and limits R12-R14 override its body): §3 (the Config payload as
published, `_device`, `Version`, `Error`, the replay markers, names), §4.4 (the Store's view of a disabled
instance), §4.5 (purge and Forget built), §5 (rewritten to the Store kit as built), §8, §10 and the new §11
(where the runtime does not yet meet an S4 obligation). S4a added no bridge-side requirement beyond what v7
already said; the wire contract is unchanged.

## 1. Problem and scope

R1 runs one device link per process. That fits **controller-fronted** bridges, one process per
controller: mqtt_ac, mqtt_hdl, mqtt_lutron, mqtt_dmx. **Device-direct** bridges must serve many
devices in one process, with one MQTT task and a task per device: mqtt_ynca (receivers) and the IR
emitter bridges (RTI processors, Global Cache units; both under the root `IRemitter`). Several
processes of one kind may share a root (bridge and controllers on the same subnet; two subnets).

In scope: the runtime host (R2), the kit's configured instances and availability (S4), the mapping
per bridge. Out of scope: restore (deferred), the IR drivers, the YNCA conversion itself.

## 2. Owner decisions

| # | Decision |
|---|---|
| D1 | Controller-fronted: one process per controller. Device-direct: one process, many devices. Both shapes are one runtime (D9): a controller-fronted process is the host with one fixed instance. |
| D2 | Prefer self-configuration (bridge enumerates) wherever the protocol allows; manual Admin UI definition is the fallback; chosen per level (instance, target), driver default. |
| D3 | A configured instance's Config carries the implementation kind (`bridge`, verified) and the serving process (`via`, selected). Several processes of one kind may share a root. |
| D4 | (revised 2026-10-10, option a) A device worker blocked past its deadline, or with a budget that stays exhausted, ends the process (R1's dead-worker path). So does a driver that panics, or a worker that ends unasked — R1's dead-worker path too (accepted at the R2 gate). Every other device failure — no reply, timeouts, a closed link — is that device's lost link, handled per device. |
| D5 | Available ⇔ the serving process's `Active` is true AND the instance's own `Status` is `connected`. |
| D6 | Shape A: one MQTT session routing to per-device runtimes. |
| D7 | The Store is hardware-agnostic: one IR device class; `IrCommand`; GC translates to raw IR in the bridge from the port's code-set Config. |
| D8 | Status and State carry their publisher (`via` user property); each device bootstraps from its own per-instance subscriptions; configured instances are tagged; a bridge clears only its own tagged topics, on delete; the Store clears only what it publishes. |
| D9 | (2026-10-10, after the R2 build) **One runtime.** The single-device `Bridge` is a thin facade over the multi-device host with N = 1: one fixed instance, named from `--instance` (§4.9). R1's separate session and process path is deleted; no duplicate connect, republish, flush or packet-id budget code remains. R1's startup and wire behaviour do not change. |

## 3. Wire contract

Grammar unchanged: `{Root}/{Lane}/{instance}[/{target}[/{property}]]`.

| Topic | Dir | Retained | Payload / meaning |
|---|---|---|---|
| `Config/{instance}` | Store → bridge | yes | **NEW.** `{"bridge":"<kind>","via"?:"<bridge id>", …driver fields…, "disabled"?:true}`. Empty = deleted. Published by the Store's Config keeper with an `origin` user property: `via` is present iff the instance names a process, `disabled` only as `true`, keys sorted. Never published for a fixed instance. |
| `Config/{instance}/{target}` | Store → bridge | yes | Unchanged (per-target setup; a GC port's code set, §5.3). |
| `Active/{bridge id}` | bridge → Store | yes, last will | Plain text `true`/`false`. Multi-device: one per process. A bridge never publishes an empty one; the Store reads an empty one (a purge or Forget cleared it) as unknown. |
| `Version/{bridge id}` | bridge → Store | yes | Plain text, one line, `<kind> <version>`, e.g. `globalcache 1.2.0`. The first word is the process's `Kind` in the Store, which refuses a `Via` whose known kind differs from the instance's. |
| `Status/{instance}` | bridge → Store | yes | Plain text `connecting`/`connected`/`unreachable`/`disabled`, **with the MQTT v5 user property `via` = the publishing bridge id.** |
| `State/{instance}/{target}` | bridge → Store | yes | JSON, **with the user property `via`**. Device-wide values use the reserved target `_device`: the Store lands them on an instance whose class is a `BridgeDeviceTarget` and ignores them (DEBUG) otherwise; it makes no child of that name. |
| `Error/{instance}` | bridge → Store | no | `{reason, error, target?}`; an instance-level error has no `target`; the Store records a `malformed`/`unusable` one in the instance's persisted `ConfigError` until it publishes a different Config. Carries no `via`: accepted from any process. |
| `Event/{instance}` | bridge → Store | no | Unchanged. |
| `Desired/…`, `Command/{instance}` | Store → bridge | as R1 | Unchanged. |

**Single-device bridges** (mqtt_ac): unchanged on the wire — no Config, no `via` tag (a fixed
instance cannot move between processes). **Configured instances:** every `Status`/`State` carries
exactly one `via` user property; none or several → ignored (DEBUG) by the Store and by read-back.
The read-back (S1, and a re-enable's seeding from the slot) takes as a "before" value only a `State`
whose one `via` is the instance's effective owner (§4.2); another process's tag is no "before" value. The Store never publishes `Config/{instance}` for a fixed instance (S4a F1: a fixed bridge's `Config/{instance}/#` also matches the parent level, and it would answer `malformed` at every replay).

Rules:
- **Bridge id** (`--name`, default = the kind) is unique on the root. `via` omitted in a Config = the
  default id of the kind the Config names (`bridge`), so a process of another kind ignores it and says
  nothing (R2 gate, Codex 9); a Config naming neither `via` nor `bridge` names no owner (unreadable).
- **Instance names are unique on the root** (all kinds, all managers of one Store MQTT client; the Store enforces it on create,
  rename, `Root` and client change, and refuses a name that fails its `wire_id_problem`: §11 G1). An instance's **kind never changes in place** (a different kind = a new object).
- **Ownership:** a process owns an instance iff its last *accepted* Config has `via` = its bridge id.
  Everything else on the root is ignored (DEBUG).
- **The Store's replay markers** sit at `Config/_s_store/_s_sync/…` (and `Desired/_s_store/_s_sync/…`), two
  levels below the lane: the root-wide `Config/+` never matches them, and `_s_store` is a sort prefix the Store
  refuses as an instance name. A topic that reaches a bridge there is no instance's.
- **Config errors** (only the process that `via` names reports them):
  - `via` names it, `bridge` is another kind → `Error/{instance}` `{reason:"malformed", error:"kind
    mismatch"}`; not served (an instance it served is released **without** the delete's cleanup:
    §4.5 clears on a delete or a reconciled absence only).
  - unparseable JSON, invalid driver fields, or a factory failure → `Error/{instance}`
    (`malformed`/`unusable`); its **last accepted Config stays in force** (none → `Status=unreachable`).
    A `disabled` Config whose driver fields differ from the last accepted one's is validated by the
    factory first (its driver dropped unused): invalid, the instance is not disabled. Until it is
    validated it touches nothing — a replacement under way (a committed Config's driver waiting to
    start, the old device stopping for it) goes on, so an invalid disable leaves the committed Config
    serving, with the disable's `Error` (§4.3). With no accepted Config there is nothing to keep, and
    the fields are checked when it is enabled.
  - an unparseable Config whose `via` cannot be read: no owner identifiable; the process that owned the
    instance before keeps its last accepted Config and logs one WARN `device_config_invalid`; other
    processes ignore it.
- **Config equality** is on the parsed, validated value — the owner, `disabled` and the driver fields,
  so `"disabled": false` written out or an explicit default `via` is no change; identical = no-op in
  every state.

## 4. Runtime: the host (R2)

### 4.1 Shape
```
                ┌──────────────── one process (bridge id "gc-lan1") ──────────────┐
MQTT broker ◄──►│ pump ─► session: router · per-instance subscriptions · lifecycle  │
                │            ├─► device "Salon"   : worker + mailbox + reports      │
                │            └─► device "Bedroom" : worker + mailbox + reports      │
                └──────────────────────────────────────────────────────────────────┘
```
- **Session (one):** pump, client, last will (`Active/{bridge id}`), `Version`, the root-wide
  subscriptions (`Config/+`), the per-instance subscriptions, the lifecycle, the publication rotation.
- **Device (one per owned instance):** R1's worker, mailbox and reports with R1's behaviour — link
  state, outage episodes, proof, catch-up, poll/probe, conflict rule, its own `Status`, its own
  blocking and helper budgets. One device runtime per instance at a time; a stopped one is never
  restarted in place — a new one is built from a fresh bootstrap (§4.3).
- **R1's per-bridge bundles** (`Shared`, `Topics`, `Reports::State`, `Batch`, the read-back) split into
  a session part and a per-device part; the algorithms are unchanged.
- **Failure domain (D4 = a):** a device whose worker thread is **blocked** past its deadline (R1's
  watchdog verdict), or whose blocking/helper budget stays exhausted, ends the **process** by R1's
  dead-worker path (bounded shutdown, failure exit, supervisor restart); so does a driver panic, or
  a worker that ends unasked (R1's dead-worker path as well). Every other failure — a device
  that does not answer, closes, times out, misbehaves — is that device's lost link, handled per device
  by R1's outage machinery; the other devices are unaffected.
- **`Bridge` (single-device) keeps its API unchanged** (`Bridge::run(driver)`); internally it is the host
  with one fixed device — the host's N = 1 mode, a facade over `Host` (D9, §4.9): one session, one
  lifecycle, one publication rotation, one packet-id budget and one shutdown flush serve both
  shapes. One in-process difference from R1 (accepted at the R2 gate): after a
  watchdog's *blocked* verdict the keeper drops the worker's stop sender, so a worker that was only
  pending stops and disconnects before `Running::stop` — observable only after a death, which ends
  the process anyway. Dropping a `Running` unstopped stops every worker, as R1's did. The multi-device API is `Host::new(kind).device(factory).run()`, where the
  factory builds a driver from an instance Config.

### 4.2 Effective owner
One rule, used for routing, read-back, filtering and cleanup:
- a **fixed** (single-device) instance → its owner is the process itself; no Config, no `via` needed;
  it is never subject to Config-absence cleanup;
- a **configured** instance → `via` if present, else the default bridge id of the kind its Config
  names (`bridge`).

### 4.3 Ownership, per-instance subscriptions, and bootstrap
- **Root-wide:** `Config/+`, then a **barrier** subscription `{Root}/_barrier/{bridge id}` (never
  published to). The barrier's SUBACK (matched by packet id) closes the root replay: the instance
  inventory is complete, even when it is empty. A refused or overdue (10 s) barrier SUBACK ends the
  session — time alone never establishes absence. A process acquires an instance when `effective_owner(Config) == this
  process` (§4.2) and the kind matches; it releases it when either stops being true.
- **Per-instance bootstrap**, three subscriptions in order, each SUBACK matched by packet id to its
  instance (R1's ordering technique, per instance; the broker sends a subscription's retained replay
  after its SUBACK and before the SUBACK of a later subscription on the same connection):
  1. **S1 read-back:** `State/{i}/+`, `Status/{i}`.
  2. **S2 setup:** `Config/{i}/+`. S2's SUBACK **closes S1's replay**: the conflict snapshot freezes
     (the read-back subscription stays installed for §4.5's healing).
  3. **S3 requests:** property `Desired/{i}/+/+` (retain-as-published off); target-level
     `Desired/{i}/+` and `Command/{i}` (retain-as-published on). S3's SUBACK **closes S2's replay**:
     target Configs are applied first, then the device becomes eligible to execute, then S3's replay
     (retained Desired) goes through the conflict rule.
  A refused or overdue (10 s) SUBACK ends the session (§4.6).
- **Packet ids** (MQTT §2.2.1: an id is not reused until its acknowledgement; R2 gate, Codex 3).
  rumqttc 0.25 hands out SUBSCRIBE, UNSUBSCRIBE and publish ids from one counter cycling through the
  broker's receive maximum, and checks only publishes against the ids in use — R1's own risk too,
  closed for a fixed bridge by the same budget (§4.9). The host therefore
  hands rumqttc a SUBSCRIBE or UNSUBSCRIBE only when no publish is unacknowledged (publishing nothing
  while one waits for that), and while any is outstanding hands out fewer ids after the oldest than
  the receive maximum — requests and publishes alike, the publication budgeted to match. Every
  publish goes through that budget: the rotation's, an instance's model republished at its S1 grant
  (into its slot's batch), a stopping instance's `connecting` (the outbox) and `Active`/`Version`
  (checked against it). A slow SUBACK thus holds publication to a few messages until it comes or its
  10 s bound ends the session. The shutdown's flush (§4.7) publishes within the same budget, waits
  for an acknowledgement for what does not fit until its ack wait's deadline, and past it publishes
  nothing more — no `Active=false`, no DISCONNECT: the last will says `Active=false`.
  Each acknowledgement matches its one request by its unique packet id.
- **Construction off to the side:** the factory runs on its own thread under the host's
  `--factory-timeout` (default 10 s; the driver's operation timing is unknown until `info()`), then
  `info()` under the same bound. A blocked factory or `info()` ends the process (D4 = a). For a Config
  **change**, the new driver is built while the current device keeps serving; only on success is the
  current device stopped (§4.4) and the new one started (the commit). A factory error → the Config
  error rule (§3): the current device keeps running unchanged.
- **First construction failure:** the instance enters **failed**: `Status=unreachable`, one
  `Error/{instance}`; requests get R1's offline policy with refusal (held property Desired expire on
  their deadline, settled by the session; Commands and target-level Desired refused with replies;
  reads refused with replies when they carry a response topic). It is retried at the reconnect pacing
  and on any Config change (a changed Config is never a no-op for a failed instance). A retry's
  construction under way is no service: the failed policy holds until a device serves. A disabled
  instance enabled again leaves Disabled before its construction (a live runtime, S2 and S3
  bootstrapped, `connecting`), so a failed construction there is this ordinary failed state.
- **A committed Config stays in force:** a Config change committed (its driver built, the current
  device stopping for it) and then changed again keeps the committed driver until the newest
  Config's construction succeeds; if that fails, the committed one starts and serves, and only the
  newest one's `Error` is said (§3, "last accepted Config stays in force").
- **Reconnect reconciliation:** after each completed `Config/+` replay (closed by the barrier), every previously owned
  instance absent from it is released as deleted (§4.4, then the bridge's cleanup, §4.5); every owned
  device's S2 replay is compared with its installed target Configs and a target Config no longer
  retained is retracted (`configure(target, None)`). "Present" is what the S2 replay brought **plus**
  the live deliveries since S2 was asked for (a target Config the Store creates in S2's window is
  retained, though no replay brought it; a live retraction removes one). An incomplete replay (§4.6 fallback) releases
  nothing and retracts nothing.

### 4.4 Stopping a device (Config change, disable, delete, release)
In order:
1. **Close admission:** the instance enters **stopping**. Deliveries for it from now on (late replay,
   pump backlog) get a terminal policy: Commands and reads with a response topic → refused, with
   reply; target-level Desired → refused; property Desired → ignored (retained; it replays to a
   successor, or on re-enable, and expires on its own deadline: a refusal would repeat at every
   replay and never settle — owner decision, 2026-10-10). Nothing is posted to the old mailbox or a
   successor.
2. **Stop the worker:** R1's stop (waiting requests answered — held Desired `superseded` on a Config
   change, `refused` otherwise — in-flight commands "cut off"), with R1's disconnect bound (2 s). A
   worker the watchdog finds **blocked** ends the process (D4 = a); a cooperative disconnect that runs
   its full bound is not a blocked verdict.
3. **Retire its output:** the device's remaining batch and earned answers stay in the session's
   rotation (State before reply, sealed as in §4.7) until published; it no longer needs the worker.
   That output is **frozen** at the worker's stop — finite — and the retirement waits for it alone;
   step 1's terminal answers to requests still arriving go to the session's outbox, a fair member of
   the rotation (§4.7), so a request flood can never hold a stop open (R2 gate, Codex 2). A
   session loss before they are published drops them with one WARN `output_lost_at_session_end`, as
   R1 drops unpublished output at process shutdown.
4. **Unsubscribe** S1–S3 (UNSUBACKs correlated by packet id; the pump forwards UNSUBACK events — an R2
   extension); an overdue UNSUBACK (10 s) ends the session. For a Config change, the successor's
   bootstrap (§4.3) starts only after the UNSUBACKs, so no old replay reaches it.
5. **Disable** stops the worker and unsubscribes only S2 and S3; the instance stays **owned**, its S1
   read-back stays subscribed, and it keeps healing its slot with `Status=disabled` (§4.5). Requests
   that still arrive (none should: S3 is gone) get step 1's terminal policy: Commands and reads with
   a response topic refused with a reply, target-level Desired refused; **property Desired ignored**
   (owner decision, 2026-10-10) — it stays retained, replays on re-enable (or to a successor) and
   expires on its own deadline. A refusal would repeat at every replay and never settle, and
   `Status=disabled` already tells the Store why nothing is applied. The Store sees a disabled instance unavailable (`Status` is not `connected`), so its pending requests keep only their `RequestExpiry` and no `ConfirmTimeout` runs (S4a §6). Re-enable runs §4.3's bootstrap
   from S2.
   Delete / release-as-deleted unsubscribes all three, then cleanup (§4.5). A `via` move unsubscribes
   all three and keeps no claim.

### 4.5 Retained topics: who may write and clear what
- **Tags:** every `Status`/`State` from a **configured** instance carries exactly one `via` user
  property. **Fixed (single-device) instances are never tagged** and need no tag — they cannot move
  between processes. No migration mode exists.
- **Self-healing slot:** while a process owns an instance (running or disabled; not while stopping), its read-back
  subscription shows later writes to its `Status/{i}` and `State/{i}/…`. A write tagged with another
  `via`, **or an empty retraction**, makes the owner republish its current value for that topic
  (coalesced: at most one pending reassertion per topic). A topic the owner has no current value for
  (a target it never reported) is left as it is. A previous owner writes a bounded number of late
  messages and stops, so this converges.
- **Bridge cleanup:** on delete (a deleted Config, or an instance released as deleted by reconnect
  reconciliation), the bridge retracts the `Status/{i}` and `State/{i}/…` topics whose **current**
  retained value carries its own tag (from its read-back). A clear that races a successor's write is
  repaired by the successor's healing. An instance whose Config names the process again while its
  delete's stop is still under way is kept, and the delete's cleanup is cancelled at the stop's end:
  its own healing covers its slot, and the new runtime's read-back meets no retraction of the values
  it reads (the R2 confirming reviews 2, Codex 2).
  - **A cleanup that cannot be done at once** (the delete ended while the broker was away, its
    retractions were lost with their connection before the broker acknowledged them) stays pending
    and is done after the next root replay (closed by the barrier), if the instance is not the
    process's again (acquired again, its retractions not yet handed to the broker are dropped: its
    own healing covers its slot, as for an instance acquired before the sweep's replay closed) — by a
    **sweep**: a short per-instance read-back (`State/{i}/+`, `Status/{i}`
    subscribed, then unsubscribed; the last UNSUBACK closes the replay) enumerates the topics and
    their current tags, and only those still carrying the process's own tag are retracted. A
    cleanup's topics are never cleared from a list cached at the delete (R2 gate, Codex 7). A
    cleanup counts as done only once the broker has acknowledged its retractions.
  - **Litter of a delete while the process was down** (amended, R2 gate, Claude 3; D4 = (a) makes a
    restart routine): at the process's **first** root replay it also reads `Status/+`, closed by the
    same barrier, then unsubscribes it. Every instance whose retained `Status` carries the process's
    own tag but which it does not own is swept as above. So a delete made while the process was
    down leaves no owner-tagged `Status`/`State` that a later re-creation under the same bridge id
    would read as current. (An instance released by a kind mismatch is swept the same way at the
    next restart.)
- **The Store clears only what it publishes** (§5.1): its Config, its Desired and its markers. Bridge-owned
  topics (`Active`, `Version`, `Status`, `State`) it clears only on a person's explicit action, built in S4a
  (§10.2, §10.3 there), on the connection it has just read and never queued:
  - **Purge** (`PurgeRetained`, on the manager by instance name or on the instance) empties the instance's
    retained `Status` and `State` whoever's tag they carry, and a foreign `Config` of that name no object
    wants. A configured instance's host heals its slot (§4.5 above), so purging a served one costs a moment. A
    **fixed** bridge subscribes no `Status` and republishes it only on a change (§11 G2), so the Store
    refuses to purge a running one.
  - **Forget** (on a bridge process) deletes the process object and empties its `Active` and `Version`; it is
    refused while `Active` is `true`, or until the current connection has shown the process down. A bridge
    that is in fact running republishes `Active=true` at its next CONNACK (§4.6).

### 4.6 Startup and liveness
On every CONNACK: subscribe `Config/+`; acquire instances as their Configs replay (each with its
§4.3 bootstrap). The session publishes, **before** `Active/{bridge id}=true`, the current lifecycle
Status of every owned instance it has reconciled (`connecting`, `connected`, `unreachable`,
`disabled`, or `unreachable` for a failed one; `connecting` for one whose device is stopping for a
successor — the stop spanning the reconnect; R2 gate, Claude minor 4) — through the publication,
within the packet-id budget (§4.3), `Active=true` waiting until they are out. Then `Active=true` is
published when

`all subscriptions requested so far are granted && (root barrier acknowledged || 5 s since the Config/+ grant)`.

A pending grant is never bypassed. With the 5 s fallback, instances whose Config arrives later get
their Status as they are acquired (accepted transient, §8).

A fixed host (a `Bridge`, §4.9) owns nothing by Config: its CONNACK subscribes no `Config/+`, waits
for no barrier, and keeps R1's startup instead — its `Active=true` once its requests are subscribed,
then its `Version` and model.

### 4.7 Publication and fairness
Per device, R1's cancellation-safe batch, **sealed** when taken (later State goes to the next batch), so
a reply follows the State its command produced and is never starved by later State. Devices are served
round-robin with a persistent cursor, at most 32 messages per device per turn, then ready inbound and
lifecycle work, then the next device. The session's outbox (instance-level `Error`s, terminal
answers, a retired runtime's answers, cleanups) is one member of that rotation, after the devices. A stopping device's flush (§4.4) takes its turns until done. At
process stop, answers first (R1), within the packet-id budget (§4.3); the model of a device not
announced on the connection (its read-back not yet granted — the fixed instance's, `Active=true`
not yet said) is not published: it would overwrite the retained State its read-back is still
reading (the R2 confirming reviews 2, Claude minor 3). A due timer (a grant's bound, a retry, the sweep) is served before
anything else at every pass, and the lifecycle's events (a construction done, a worker stopped) go
ahead of the inbound traffic: neither a steady publication nor a steady inbound stream can starve
them (R2 gate, Codex 1).

### 4.8 Logging
Every record under a device carries `instance` and the bridge id. New kinds: `device_config_invalid`
(WARN), `device_config_rejected` (WARN), `subscription_refused` (WARN), `slot_reasserted` (INFO, a
foreign write seen on an owned slot).

### 4.9 The fixed instance: the host's N = 1 mode (D9)
There is one runtime. A `Bridge` is a thin facade over `Host` (its builder delegates; its command
line is R1's: `--instance`, no `--name`, no `--factory-timeout`), and the host serves exactly one
**fixed instance**, named from `--instance`:
- **Ownership:** the process itself (§4.2). No `Config/{instance}` is read for it and no `Config/+`
  is subscribed, so there is no root barrier, no reconnect reconciliation, no `Status/+` litter scan;
  its bridge id is its instance (`Active/{instance}`, `Version/{instance}`, the client id
  `{Root}-{instance}`, the last will).
- **No `via`:** its `Status` and `State` are untagged (§3, §4.5); it never heals or cleans its slot.
- **Its driver** is the one `main` hands to `run` — no factory, no `--factory-timeout` — and its
  worker starts at once, as R1's did: the device side runs whatever the broker does, its reports
  waiting for `Active`.
- **Its CONNACK duties are R1's, in R1's order** (Bridge Runtime spec §5, §9), carried by the host's
  lifecycle: once per process the read-back `State/{instance}/#`, alone; its SUBACK awaited within
  `READ_BACK_WAIT` (5 s) — refused or late: WARN `read_back_incomplete` and on without it; late, the
  session ends at the wait, since only a new connection frees the abandoned SUBSCRIBE's packet id,
  and the next connection subscribes the requests at once (the R2 confirming reviews 2, Claude
  minor 2; R1 went on on the same connection) — and, granted, the next SUBACK (behind its replay),
  whichever request it acknowledges, granted or refused, completes it, after which it is
  unsubscribed; a session that ends
  before its SUBACK leaves it to the next, one that ends mid-replay leaves it incomplete. Then the
  four request SUBSCRIBEs, `Config/{instance}/#`, `Desired/{instance}/+` (retain-as-published),
  `Desired/{instance}/+/+`, `Command/{instance}` (retain-as-published); once they are handed over
  (granted or not: R1 never waited for them), `Active=true`, the `Version`, and the whole retained
  model, then its reports. Every publish is routed as R1's (its grammar answers what is none of its
  lanes on `Error`).
- **The same machinery:** every SUBSCRIBE, UNSUBSCRIBE and publish of it goes through §4.3's
  packet-id budget, which closes R1's own MQTT §2.2.1 risk (`Active`, `Version` and the model used to
  go out right behind the subscriptions, on ids a SUBSCRIBE still held); its output goes through
  §4.7's rotation; the shutdown's flush is §4.7's, budgeted (the model of a slot not yet announced —
  before `Active=true` — is not published at the stop: it would overwrite the retained State the
  read-back is still reading). As every request does, an outstanding SUBSCRIBE or UNSUBSCRIBE of it
  is bounded by §4.3's 10 s (an overdue one ends the session — otherwise the budget would freeze
  publication once the id space wraps); a refusal of one of its request subscriptions is not read,
  as R1 did not.

## 5. Store kit (S4)

**Built as S4a** (Store main c6360a333; `docs/superpowers/specs/2026-10-11-bridge-kit-s4a-design.md`, whose §19
deviations override its body), on S2 (`RetainedIntent`) and S3 (`#Std/Availability`). S4b, the pair
migrations (mqtt_ynca with `ynca.sdl` first), is still to come: mqtt_ynca today ignores `bridge`/`via` and serves
every `Config/#` under its root, so no kit manager may sit on the `Ynca` root until then (S4a F15).

### 5.1 Instances: discovered or configured (D2)
- **Discovered** (CoolMaster): unchanged, except that `Active` and a lone `Status` no longer create an instance
  (each `Active`/`Version` makes a process object, below): an untagged `State` does, unless the process is
  known down. A family is all discovered or all configured.
- **Configured** (YNCA, RTI, GC): the user creates the instance (address, `Via`, Disabled…). The **Config
  keeper** `#Mqtt/BridgeConfigs` (one per MQTT client, made by the first kit manager on it) publishes
  `Config/{instance}` and `Config/{instance}/{target}`, retained, with the user property `origin`. It is not a
  blind re-assert: it reads each root back on every connection (a marker at `Config/_s_store/_s_sync/…`) and
  decides every publish and retraction from that read-back, bound to the connection that read it. An identical
  Config of its origin is adopted (no re-send, no bridge churn after a Store restart); one the broker lost is
  sent again; one of its origin that no object wants is retracted (a deleted instance or manager, a rename, a
  `Root` or client change, an earlier run's); another origin's is left alone, and creating an object of that
  name takes it over. A 60 s sweep catches what no event announces.
- **Uniqueness** of names per root and client, across managers, refused at create, rename, `Root` and client
  change; the kind is fixed by the class. A fixed instance never gets a `Config/{instance}`.
- **Delete / rename:** the Store retracts only its own topics (`Config/{i}`, `Config/{i}/{t}`, its retained
  `Desired/{i}/…`, and the orphaned `Desired` of a deleted manager or an old root). A rename gives up every
  pending request of the instance. Bridge-owned `Status`/`State` are cleaned by their owner (§4.5). **Reuse of
  a deleted name** is not refused: the instance is created at once and its Config is *held* until the
  retractions of the earlier owner's `Desired` under that name are delivered, so no old request replays to the
  new device (S4a §5.5).
- **Changing the device behind an instance** (a `device_identity_fields()` field, e.g. its address): the Store
  retracts the instance's and its targets' pending `Desired` before the new Config goes out (held until they
  are delivered).
- **Targets:** discovered by default; a driver may declare them configured (HDL): `Config/{i}/{t}`.
- **Unload** (S4a R12): a real store keeps its Configs on the broker through unload, reset and restore; its
  delete, or its MqttClient's, clears them; a test store (name starting `_`) clears everything. Deployment
  assumption: one store per broker and root (R13, R14 are the limits when that fails).

### 5.2 Availability (D5)
- Per bridge id: each `Active/{bridge id}` or `Version/{bridge id}` makes a `#Mqtt/Bridge` process object in the
  manager's `_z_Bridges` folder (`Active` true / false / unset, `Kind` = the first word of `Version`), fixed
  bridges included. A `Root` change resets them.
- A **configured** instance accepts `Status/{i}` and `State/{i}/…` only when the message carries exactly one
  `via` equal to its effective owner (§4.2). A **fixed** instance accepts untagged messages (tags, if any, are
  ignored). Messages from any other process are ignored (DEBUG `bridge_via_ignored`), so a late write from a
  previous owner can never stand.
- `Via` is a reference to a process object, picked in the Admin UI's reference picker (a whole-store tree
  narrowed by class; no hook can offer only matching processes). A write is refused for a process under another
  manager or of another known kind. The id on the wire is the hidden `ViaId`, kept when the process object is
  deleted (which clears `Via`), so nothing changes on the wire. With exactly one process of the kind, `Via` is
  set to it at creation; unset, the owner is the kind's default id.
- On a change of `Via` the instance's `LinkStatus` resets to unknown until the new owner's Status arrives.
- `Available = active(effective owner) == true && LinkStatus == "connected"`; unknown counts as not available.
  Recomputed on a change of `Via`, the effective Root, a process's `Active` or `LinkStatus`, and fanned out
  through the kit's existing `push_link` to targets and bound equipment; a newly discovered target starts from
  its instance's current availability.

### 5.3 IR devices (D7)
- One device class (`IrDevice`) sets `IrCommand` → momentary `Command/{controller}`
  `{"target":"<port>","command":"ir","code":"<IrCommand>"}` (the kit's flat convention).
- Controller classes (`RtiController`, `GlobalCacheController`) carry address, kind and `Via`.
- Port discovery: `State/{controller}/{port}` `{"kind":"ir_port","index":n,"label"?:s}`; stable port
  ids (`ir1`…, GC `"{module}:{port}"`).
- Port Config: `Config/{controller}/{port}` `{"codeset":{"name":s,"version":n,"codes":{"<IrCommand>":
  "<raw>"}}}` (GC needs `codes`; RTI needs `name`). Outcomes: no code set → `refused`; unknown code →
  `rejected` naming it; invalid code set → `Error` naming the port, previous code set stays.

### 5.4 What the Store does with an instance it does not configure (S4a)
- **Adoption.** In a configured family, an instance the broker shows that no object has is a candidate
  (`#Mqtt/DiscoveredInstance` in the manager's `_y_Discovered` folder) when, on the Store's current
  connection, a `Config/{i}` of another origin or none is read back (a predecessor store, a pre-kit driver), or a
  `via`-tagged `Status`/`State` arrives for the name. Its kind is the Config's `bridge`, else the serving
  process's `Kind`; with neither, `Adopt` is refused. `Adopt` makes the instance (`ViaId` = the seen process,
  fields from the Config) and the keeper publishes the Store's Config over the old one; `AdoptAll` does every
  candidate not ignored. A fixed family has none. A candidate goes when its source is cleared, an instance of its
  name appears, or a new connection's replay does not bring it back.
- **Purge and Forget:** §4.5.

## 6. Mapping

| Bridge | Process | Instance | Source | Targets | Source | Bridge id |
|---|---|---|---|---|---|---|
| mqtt_ac | per CoolMaster | controller | discovered | AC units | discovered | the instance |
| mqtt_hdl | per IP interface | interface | discovered | HDL devices | configured (discovered if possible) | the instance |
| mqtt_lutron | per processor | processor | discovered | devices | decide at conversion | the instance |
| mqtt_dmx | per bridge | the bridge | discovered | universes, arrays | configured | the instance |
| mqtt_ynca | per subnet | receiver | configured | zones, `_device` | discovered | default `ynca` |
| IR: RTI | per subnet | processor | configured | IR ports | discovered | default `rti` |
| IR: Global Cache | per subnet | GC unit | configured | IR ports | discovered (+ code set) | default `globalcache` |

## 7. Testing (each scenario failing-first, with an outer deadline and a progress assertion)
1. Two devices: one never replies — it goes `unreachable` and retries; the other answers within 1 s.
2. Acquire: a Config naming this process subscribes the instance's filters; the broker's replay of its
   retained target Configs and Desired bootstraps the device (target Configs first, conflict rule).
3. Change / disable / delete: admission closes first (late deliveries get the terminal policy); the
   worker stops (R1); its output retires through the rotation; UNSUBACKs before a successor bootstraps;
   a failing new factory leaves the current device serving; delete clears only its own tagged topics.
4b. Reconnect reconciliation: an instance deleted while disconnected is released after the replay; a
   removed target Config is retracted; an incomplete replay changes nothing.
4c. Bootstrap order: read-back freezes at S2's SUBACK (retained State behind S1's SUBACK is in the
   snapshot); target Configs apply before retained Desired.
4. Blocked worker or factory, or an exhausted budget → the process ends with a failure exit (D4 = a);
   the other devices' Status are restored by the restart, not left stale.
5. Startup: `Active=true` only after all grants; a refused `Config/+` or instance subscription ends the
   session; current statuses republished after startup.
6. Two processes of one kind on one root: moving `via` — the new owner bootstraps by its subscriptions;
   a late Status from the old owner is reasserted over by the new owner and ignored by the Store.
7. Fairness: a device producing State continuously never delays its own reply nor another's.
8. Last will: process death → that bridge id's instances unavailable; other processes unaffected.
9. The fixed instance (§4.9): R1's conformance suite unchanged and passing on the host; no packet id
   reused while one of its SUBACKs is held (failing first against R1's own session); a late
   read-back said incomplete and gone without; no root subscription.
Kit DSL tests: Config via RetainedIntent; uniqueness; delete retracts only Store-owned topics;
Status/State accepted only from the effective owner, exactly one tag;
LinkStatus reset on `Via` change; availability truth table; IR shapes.

## 8. Risks
- A blocked device worker (a driver bug) restarts the whole process: its other devices drop for a few
  seconds (D4 = a, accepted by the owner).
- A Config change drops that device's link briefly (stop, rebuild, re-subscribe).
- A `via` move takes the instance briefly offline; a late old-owner write or clear is repaired by the new
  owner's healing (bounded).
- Litter from a permanently removed bridge stays until a person runs the Store's purge or Forget (built in S4a, §4.5): never an automatic janitor.
- The 5 s Config-replay fallback (§4.6) can show a stale Status for an instance whose Config has not
  yet arrived.
- YNCA: one connection per receiver; live testing needs the old system's session released.
- A fixed bridge (§4.9) whose broker never acknowledges one of its SUBSCRIBEs or UNSUBSCRIBEs now
  ends the session after 10 s and reconnects (R1 went on regardless) — its read-back's SUBACK, after
  its 5 s wait, so that a broker slow on it (5 to 10 s) costs one reconnect; with a broker that
  answers its acknowledgements, nothing differs. A publication held while an UNSUBSCRIBE waits for the line to clear of publishes (the
  read-back's, just after `Active`) resumes once the broker acknowledges what is in flight.

## 9. Documentation (every level, both sides)
Reference and tutorial for the Rust side (rustdoc, runtime spec, crate README with D1, "Writing a
bridge": mqtt_ac v2 single-device, mqtt_ynca multi-device) and the SDL side (kit spec, bridge contract
guide, mixin doc comments, "Writing an SDL driver on the kit": discovered vs configured, `Via`,
availability, IR); the tutorial artifact and the `developing-mqtt-bridges` skill in sync.

## 10. Sequencing
1. Land R1. 2. Approve this spec. 3. Kit S2 (RetainedIntent) and S3 (Availability). 4. R2 runtime host. 5. S4 kit: **S4a landed** (Store main c6360a333: the keeper, processes, `via`, adoption, purge); S4b (the pair migrations) follows. 6. mqtt_ynca + ynca.sdl (S4b). 7. IR bridges (own designs). 8. The runtime gaps of §11.

## 11. Open gaps: where the runtime as built does not yet meet an S4 obligation
Checked against `mqtt-bridge-kit` at tracing-init master a016fe1 and Store main c6360a333 (2026-10-12). Met, so
not gaps: the `via` tag on every configured `Status`/`State`; property `Desired` ignored while stopping or
disabled (`lifecycle.rs` `terminal()`); a late fixed read-back ending the session; `Config/+` one level, so the
Store's markers never match; the kind as the first word of `Version`; per-instance Config errors without
`target`; a bridge never publishing an empty `Active`.

- **G1. No wire-id validation beyond "one segment".** The Store makes an object only for a bridge id or instance
  name that passes its `wire_id_problem` (`Std/mqtt_bridge.sdl`): not whitespace-only, `.` or `..`, a leading
  `@`, `*` or `{…}`, a trailing `@` or digit+`_`, or a sort prefix `_x_…`. The runtime accepts any
  `valid_segment` (`topics.rs`, `config.rs`) for `--name` and `--instance`, so a bridge started as `_z_Bridges`
  or `gc-1_` runs, publishes, and is never shown by the Store. Obligation: refuse such a name at start-up (a
  usage error), with the Store's list. A configured instance's name is the Store's to refuse; only the bridge id
  and a fixed instance's name are the runtime's.
- **G2. A fixed bridge does not heal its slot.** Hosts republish a topic that is emptied or tagged by another
  process (§4.5), but a fixed bridge subscribes no `Status` and publishes it only on a change (`reports.rs`,
  `lifecycle.rs`): a Store purge of a running fixed bridge would leave its `LinkStatus` unset until the link next
  changes. The Store works around it (it refuses that purge, S4a D39), so nothing is wrong on the wire today; the
  consequence is that a retained `Status` lost on the broker (a broker restart without persistence is covered by
  the CONNACK republish, a person's manual clear is not) is not repaired until the next change.
- **G3. A fixed bridge answers a stray `Config/{instance}`.** Its `Config/{instance}/#` subscription also matches
  the parent level, which parses as `Unknown` and is answered with `Error` `malformed` at every replay (S4a F1,
  inferred from `topics.rs`/`session.rs`, no test). The Store never publishes it, so it is silent in practice;
  another publisher's would make the bridge say `malformed` on every connection. Hardening: ignore (DEBUG) the
  parent level for a fixed instance.
- **G4. `Error` carries no `via`.** The Store cannot attribute an instance-level `Error` to a process (S4a F2,
  R6): after a `Via` move an old owner's late refusals abandon pending requests, and a grammar `Error` of a
  process that does not own the instance could be recorded as its `ConfigError`. Accepted by S4a as bounded; it
  would close if `Error` carried the tag too, a wire change for a later decision.
