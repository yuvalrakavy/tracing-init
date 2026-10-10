# Multi-device bridges — design v7 (Bridge Runtime R2 + Bridge Kit S4)

Status: APPROVED by the owner 2026-10-10 (v7) (v6 + its gate's 3 findings; that gate closed the rest and found no new shape issue). Three design gates (v1, v3, v4) kept finding races in the
per-device isolation machinery (generations, input cache, revocation, cross-process cleanup). The
owner switched D4 to (a): a stuck device worker restarts the process; every other device failure stays
per device. With that, v5 drops generations, the input cache, revocation and the leaked-thread ledger,
and bootstraps each device from its own per-instance subscriptions (a genuine broker replay).
Tier T2 (it mints wire contract).

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
| D1 | Controller-fronted: one process per controller. Device-direct: one process, many devices. |
| D2 | Prefer self-configuration (bridge enumerates) wherever the protocol allows; manual Admin UI definition is the fallback; chosen per level (instance, target), driver default. |
| D3 | A configured instance's Config carries the implementation kind (`bridge`, verified) and the serving process (`via`, selected). Several processes of one kind may share a root. |
| D4 | (revised 2026-10-10, option a) A device worker blocked past its deadline, or with a budget that stays exhausted, ends the process (R1's dead-worker path). Every other device failure — no reply, timeouts, a closed link — is that device's lost link, handled per device. |
| D5 | Available ⇔ the serving process's `Active` is true AND the instance's own `Status` is `connected`. |
| D6 | Shape A: one MQTT session routing to per-device runtimes. |
| D7 | The Store is hardware-agnostic: one IR device class; `IrCommand`; GC translates to raw IR in the bridge from the port's code-set Config. |
| D8 | Status and State carry their publisher (`via` user property); each device bootstraps from its own per-instance subscriptions; configured instances are tagged; a bridge clears only its own tagged topics, on delete; the Store clears only what it publishes. |

## 3. Wire contract

Grammar unchanged: `{Root}/{Lane}/{instance}[/{target}[/{property}]]`.

| Topic | Dir | Retained | Payload / meaning |
|---|---|---|---|
| `Config/{instance}` | Store → bridge | yes | **NEW.** `{"bridge":"<kind>","via":"<bridge id>", …driver fields…, "disabled"?:bool}`. Empty = deleted. |
| `Config/{instance}/{target}` | Store → bridge | yes | Unchanged (per-target setup; a GC port's code set, §5.3). |
| `Active/{bridge id}` | bridge → Store | yes, last will | Plain text `true`/`false`; empty = unknown. Multi-device: one per process. |
| `Version/{bridge id}` | bridge → Store | yes | Plain text `<kind> <version>`, e.g. `globalcache 1.2.0`. |
| `Status/{instance}` | bridge → Store | yes | Plain text `connecting`/`connected`/`unreachable`/`disabled`, **with the MQTT v5 user property `via` = the publishing bridge id.** |
| `State/{instance}/{target}` | bridge → Store | yes | JSON, **with the user property `via`**. Device-wide values use the reserved target `_device`. |
| `Error/{instance}` | bridge → Store | no | `{reason, error, target?}`; an instance-level error has no `target`. |
| `Event/{instance}` | bridge → Store | no | Unchanged. |
| `Desired/…`, `Command/{instance}` | Store → bridge | as R1 | Unchanged. |

**Single-device bridges** (mqtt_ac): unchanged on the wire — no Config, no `via` tag (a fixed
instance cannot move between processes). **Configured instances:** every `Status`/`State` carries
exactly one `via` user property; none or several → ignored (DEBUG) by the Store and by read-back.
The read-back (S1, and a re-enable's seeding from the slot) takes as a "before" value only a `State`
whose one `via` is the instance's effective owner (§4.2); another process's tag is no "before" value.

Rules:
- **Bridge id** (`--name`, default = the kind) is unique on the root. `via` omitted in a Config = the
  default id of the kind the Config names (`bridge`), so a process of another kind ignores it and says
  nothing (R2 gate, Codex 9); a Config naming neither `via` nor `bridge` names no owner (unreadable).
- **Instance names are unique on the root** (all kinds, all managers; the Store enforces it on create
  and rename). An instance's **kind never changes in place** (a different kind = a new object).
- **Ownership:** a process owns an instance iff its last *accepted* Config has `via` = its bridge id.
  Everything else on the root is ignored (DEBUG).
- **Config errors** (only the process that `via` names reports them):
  - `via` names it, `bridge` is another kind → `Error/{instance}` `{reason:"malformed", error:"kind
    mismatch"}`; not served (an instance it served is released **without** the delete's cleanup:
    §4.5 clears on a delete or a reconciled absence only).
  - unparseable JSON, invalid driver fields, or a factory failure → `Error/{instance}`
    (`malformed`/`unusable`); its **last accepted Config stays in force** (none → `Status=unreachable`).
    A `disabled` Config whose driver fields differ from the last accepted one's is validated by the
    factory first (its driver dropped unused): invalid, the instance is not disabled. With no
    accepted Config there is nothing to keep, and the fields are checked when it is enabled.
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
  dead-worker path (bounded shutdown, failure exit, supervisor restart). Every other failure — a device
  that does not answer, closes, times out, misbehaves — is that device's lost link, handled per device
  by R1's outage machinery; the other devices are unaffected.
- **`Bridge` (single-device) keeps its API unchanged** (`Bridge::run(driver)`); internally it is the host
  with one fixed device. The multi-device API is `Host::new(kind).device(factory).run()`, where the
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
  broker's receive maximum, and checks only publishes against the ids in use. The host therefore
  hands rumqttc a SUBSCRIBE or UNSUBSCRIBE only when no publish is unacknowledged (publishing nothing
  while one waits for that), and while any is outstanding hands out fewer ids after the oldest than
  the receive maximum — requests and publishes alike, the publication budgeted to match. A slow
  SUBACK thus holds publication to a few messages until it comes or its 10 s bound ends the session.
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
   `Status=disabled` already tells the Store why nothing is applied. Re-enable runs §4.3's bootstrap
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
  repaired by the successor's healing.
  - **A cleanup that cannot be done at once** (the delete ended while the broker was away, its
    retractions were lost with their connection before the broker acknowledged them) stays pending
    and is done after the next root replay (closed by the barrier), if the instance is not the
    process's again — by a **sweep**: a short per-instance read-back (`State/{i}/+`, `Status/{i}`
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
- **The Store clears only what it publishes** (§5.1). Litter left by a bridge that is permanently gone
  (decommissioned) is removed by an explicit Admin UI action ("purge retained for instance"), a
  follow-up — not an automatic janitor.

### 4.6 Startup and liveness
On every CONNACK: subscribe `Config/+`; acquire instances as their Configs replay (each with its
§4.3 bootstrap). The session publishes, **before** `Active/{bridge id}=true`, the current lifecycle
Status of every owned instance it has reconciled (`connecting`, `connected`, `unreachable`,
`disabled`, or `unreachable` for a failed one). Then `Active=true` is published when

`all subscriptions requested so far are granted && (root barrier acknowledged || 5 s since the Config/+ grant)`.

A pending grant is never bypassed. With the 5 s fallback, instances whose Config arrives later get
their Status as they are acquired (accepted transient, §8).

### 4.7 Publication and fairness
Per device, R1's cancellation-safe batch, **sealed** when taken (later State goes to the next batch), so
a reply follows the State its command produced and is never starved by later State. Devices are served
round-robin with a persistent cursor, at most 32 messages per device per turn, then ready inbound and
lifecycle work, then the next device. The session's outbox (instance-level `Error`s, terminal
answers, a retired runtime's answers, cleanups) is one member of that rotation, after the devices. A stopping device's flush (§4.4) takes its turns until done. At
process stop, answers first (R1). A due timer (a grant's bound, a retry, the sweep) is served before
anything else at every pass, and the lifecycle's events (a construction done, a worker stopped) go
ahead of the inbound traffic: neither a steady publication nor a steady inbound stream can starve
them (R2 gate, Codex 1).

### 4.8 Logging
Every record under a device carries `instance` and the bridge id. New kinds: `device_config_invalid`
(WARN), `device_config_rejected` (WARN), `subscription_refused` (WARN), `slot_reasserted` (INFO, a
foreign write seen on an owned slot).

## 5. Store kit (S4)

**Prerequisites:** S2 (`RetainedIntent`) and S3 (`#Std/Availability`) of the kit spec land first.

### 5.1 Instances: discovered or configured (D2)
- **Discovered** (CoolMaster): unchanged.
- **Configured** (YNCA, RTI, GC): the user creates the instance (address, `Via`, Disabled…). It
  publishes `Config/{instance}` through `RetainedIntent` (reasserted on the Store client's reconnect;
  pending retractions kept until delivered).
- **Uniqueness** on the root; the kind is fixed by the class.
- **Delete / rename:** the Store retracts only its own topics (`Config/{i}`, `Config/{i}/#`, its
  retained `Desired/{i}/…`) through `RetainedIntent`. Bridge-owned `Status`/`State` are cleaned by
  their owner (§4.5). Reuse of a deleted name waits until the Store's own retractions for it are delivered
  (store-wide fence across managers and clients); bridge-owned topics need no fence — the new owner's
  healing repairs any late clear from the old one.
- **Changing the device behind an instance** (its address): the Store retracts the instance's retained
  Desired before publishing the new Config, so nothing meant for the old device replays to the new one.
- **Targets:** discovered by default; a driver may declare them configured (HDL).

### 5.2 Availability (D5)
- Per bridge id, the manager keeps liveness from `Active/{bridge id}`: true / false / unknown. A Root
  change resets it.
- A **configured** instance accepts `Status/{i}` and `State/{i}/…` only when the message carries
  exactly one `via` equal to its effective owner (§4.2). A **fixed** instance accepts untagged
  messages (tags, if any, are ignored). Messages from any other process
  are ignored (DEBUG) — so a late write from a previous owner can never stand.
- On a change of `Via` the instance's `LinkStatus` resets to unknown until the new owner's Status
  arrives.
- `Available = active(effective owner) == true && LinkStatus == "connected"`; unknown counts as not available.
  Recomputed on a change of `Via`, the effective Root, the liveness map or `LinkStatus`, and fanned out
  through the kit's existing `push_link` to targets and bound equipment; a newly discovered target
  starts from its instance's current availability.
- The Admin UI offers for `Via` the bridge ids seen whose `Version` names the instance's kind.

### 5.3 IR devices (D7)
- One device class (`IrDevice`) sets `IrCommand` → momentary `Command/{controller}`
  `{"target":"<port>","command":"ir","code":"<IrCommand>"}` (the kit's flat convention).
- Controller classes (`RtiController`, `GlobalCacheController`) carry address, kind and `Via`.
- Port discovery: `State/{controller}/{port}` `{"kind":"ir_port","index":n,"label"?:s}`; stable port
  ids (`ir1`…, GC `"{module}:{port}"`).
- Port Config: `Config/{controller}/{port}` `{"codeset":{"name":s,"version":n,"codes":{"<IrCommand>":
  "<raw>"}}}` (GC needs `codes`; RTI needs `name`). Outcomes: no code set → `refused`; unknown code →
  `rejected` naming it; invalid code set → `Error` naming the port, previous code set stays.

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
Kit DSL tests: Config via RetainedIntent; uniqueness; delete retracts only Store-owned topics;
Status/State accepted only from the effective owner, exactly one tag;
LinkStatus reset on `Via` change; availability truth table; IR shapes.

## 8. Risks
- A blocked device worker (a driver bug) restarts the whole process: its other devices drop for a few
  seconds (D4 = a, accepted by the owner).
- A Config change drops that device's link briefly (stop, rebuild, re-subscribe).
- A `via` move takes the instance briefly offline; a late old-owner write or clear is repaired by the new
  owner's healing (bounded).
- Litter from a permanently removed bridge stays until the Admin UI purge action (follow-up).
- The 5 s Config-replay fallback (§4.6) can show a stale Status for an instance whose Config has not
  yet arrived.
- YNCA: one connection per receiver; live testing needs the old system's session released.

## 9. Documentation (every level, both sides)
Reference and tutorial for the Rust side (rustdoc, runtime spec, crate README with D1, "Writing a
bridge": mqtt_ac v2 single-device, mqtt_ynca multi-device) and the SDL side (kit spec, bridge contract
guide, mixin doc comments, "Writing an SDL driver on the kit": discovered vs configured, `Via`,
availability, IR); the tutorial artifact and the `developing-mqtt-bridges` skill in sync.

## 10. Sequencing
1. Land R1. 2. Approve this spec. 3. Kit S2 (RetainedIntent) and S3 (Availability). 4. R2 runtime host.
5. S4 kit. 6. mqtt_ynca + ynca.sdl. 7. IR bridges (own designs).
