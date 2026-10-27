# Exact recovery retires asynchronous observations

The resource prerequisite depth review at `53b7a4c` found an S2 defect:
an asynchronous apply or refresh could complete after exact recovery replacement
and overwrite its model or conflict cache when target generations stayed equal.
All three independent reproduction tests failed before the correction and passed
after it. Generation equality alone cannot distinguish those two observations.

Each SDK-owned asynchronous apply/read now captures a private identity before
awaiting its driver. Exact recovery replacement rotates that identity under the
model lock. All context-bound model/cache updates check it under the same lock;
stale completion therefore cannot pass validation and mutate afterward. All four
read-fold sites stamp the original identity, as do apply success and partial-result
paths. The lease changes neither generation equality nor wire metadata. Existing
opaque initiating-command completion handling remains separate.

The retained model test also checks a different Reports owner's identity and proves
that fresh observations with the unchanged generation remain accepted. Background
driver reports that deliberately construct a fresh context are not claimed to be
fenced by this private SDK completion mechanism.

Verification: full runtime 303/303, compatibility 176/176, plus the corrected model
assertion test independently green. Four guard-removal controls fail for their
named stale-update assertion and are restored. The first two control attempts
initially failed at a later unwrap after removal; their assertion order was corrected
and only those rows rerun. Those initial outcomes are not counted as valid controls.
Confirming review over the full engine/runtime fix delta remains pending.
