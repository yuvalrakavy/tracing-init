# Resource prerequisite test-validity pass

Frozen input: f94d510. GPT-6.1-sol/high, isolated mutation checkout; engine memory
counterpart reviewed at 53ae4a9. This is a prerequisite review, not the final DMX gate.

Two SDK accounting mutations survived the seven existing mailbox-budget tests:
omitting correlation contents and omitting captured RequestContext contents. New
probes isolate a 16 KiB correlation beside a one-byte command/read and a bounded
generation/request-id context beside otherwise admissible small parameters.
Both detect refusal, preservation of prior work and subsequent capacity availability.

The author retained the probes with producer-consistent read command names,
independently ran all nine mailbox-budget tests GREEN, and reproduced each exact
mutant RED through the two new resource-review control rows. All-target clippy
passes. Production code was unchanged; the earlier full runtime/compatibility
results carry from f94d510. Independent model-family coverage was unavailable.
Depth review and actual DMX managed-owner integration remain pending.
