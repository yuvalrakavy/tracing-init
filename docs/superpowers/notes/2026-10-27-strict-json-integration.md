# Strict ordinary JSON input for the DMX adapter

Integration found that the generic Value decoder accepts duplicate keys and keeps
the last value. Once Config or Command reaches a driver, that evidence is lost.
The approved DMX contract rejects duplicates, so validation must precede decoding.

`Bridge::strict_json()` opts a fixed bridge into an additional request validation
pass. Every ordinary target Config, Desired-property/target, and Command decoder
receives the flag. It visits arrays/values without constructing a second JSON tree,
holding decoded keys only for each currently visited object. Escaped aliases count
as duplicates; sibling objects may reuse keys. Empty retractions still reach the
existing decoder. It neither inspects JSON strings nor applies driver schemas or
canonical ordering. State/recovery handling is unchanged; owners validate their
recovery documents independently. Existing bridges default to their prior behavior.

Three initial visitor tests were observed RED against the stub. Five strict-mode
tests pass, including an actual launched bridge/FakeBroker case comparing default
legacy acceptance with strict rejection on all three request lanes. Refusals keep
request IDs and command correlation, valid following requests succeed, and MQTT
does not reconnect. Four guard controls are valid RED and restored. The duplicate
guard's first mutant failed to compile because it erased BTreeSet's inferred type;
it was corrected to retain insertion while ignoring its result, then only that row
was rerun. The failed compilation is not counted as a valid control.

Full runtime 308/308, unchanged compatibility 176/176, all-target clippy with warnings
denied and wait-registry tests pass. Only owned loopback brokers were used. Review
completed on frozen `ef2f12c`. The independent mutation pass detected all three
additional mutants: missing end-of-input check, skipped array traversal and lost
malformed-command correlation. Baseline and restored strict suites passed 5/5 in
its isolated archive. Depth review found no further issues in the visitor, routing,
admission bookkeeping, metadata or expiry flow. Reviews used GPT-6.1 Sol and
GPT-6 Astra; no different model-family coverage is available. The DMX adapter may
now pin this reviewed option; service-level acceptance remains pending.
