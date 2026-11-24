# Bring an existing audience

A new installation can start with zero recipients. An operator moving an established list can import an all-status export from beehiiv or map another provider’s records to the same import format. A migration is complete when the operator can explain exactly who may be contacted, demonstrate an opt-out that still works and reconcile the send ledger.

## Stage and inspect

1. Record each input’s source, UTC cutoff and SHA-256 checksum. Gather all statuses, any separate opt-out, complaint and bounce records, and consent evidence. For an existing sender also inventory accepted, pending and scheduled sends. Keep these materials in private storage. [beehiiv exports](https://www.beehiiv.com/support/article/12258595483543-exporting-post-content-or-subscriber-data-from-beehiiv) · [SES suppression lists](https://docs.aws.amazon.com/ses/latest/dg/sending-email-suppression-list.html)
2. Upload the files through the web console or MCP with `file_begin`, `file_write` and `file_finish`. Check whole-file hashes and map the actual headers. Preserve known confirmation times and provenance. Resolve recipients using consistent address normalization and prior identifiers where available. Leave unproven addresses ineligible.
3. Run `import_prepare`. Inspect its count of eligible, duplicate, suppressed, pending, held, test-only and unresolved records. Account for every input row. Suppression records win over imported active rows. Resolve collisions before proceeding. Record the sorted eligible set’s digest privately.
4. Commit the reviewed plan and inspect the job receipt. A cross-interface replay with the same identity and key must give the same outcome. Importing prior activity cannot create an email send or a new-acquisition event.

## Synthetic reconciliation

The letters below stand for distinct test identities. The row for B occurs twice in the input.

| Identity | Evidence | Expected result |
| --- | --- | --- |
| A | Active with a trustworthy opt-in record | Eligible |
| B | Older active entry and subsequent unsubscribe | One suppressed identity |
| C | Permanent bounce | Suppressed |
| D | Complaint | Suppressed |
| E | Pending confirmation | Pending |
| F | No consent evidence | Held |
| G | Explicit test contact | Test-only |

Eight source rows produce seven identities and one eligible recipient. Reverse the row order and repeat the rehearsal from a clean state. Apply the reviewed files twice. In each run, only A is eligible. Opt A out after preparing a send and check that no message reaches the local capture sink for A.

## Cut over

Choose an operator-owned cutoff and temporarily pause sending. Establish who handles new signup requests, prior-provider links and each already planned campaign. Reconcile the last delta at the changeover and record the watermark with `migration_reconcile`. Carry forward consent times, suppression reasons, historical send receipts, and optional tracked campaign information. Historical click series retain their provider and measurement definition.

Make legacy unsubscribe and pending-confirmation links actionable for as long as older messages circulate. Ingest authenticated updates from the old system or reconcile recent verified exports before dispatch. Apply a subsequent unsubscribe even if a legacy active entry arrives afterward. When a historic confirmation has expired, the recipient can request a new challenge. Keep stale or conflicting records held until resolved.

Verify a new signup, a confirm, a repeated confirm and an unsubscribe, including an action from an old email. Reconcile the final eligible-set digest and confirm a single dispatch owner. Enable the intended send only after the operator can account for every changed record.

## Roll back

Pause dispatch and fence queued work. Preserve ongoing consent, unsubscribe and provider feedback routes. Export post-cutoff suppressions, confirmations and the delivery ledger. When resuming with a previous sender, apply suppressions first, then verifiable positive consent. Never release a campaign from another sender for a recipient whose prior delivery was accepted or is still unknown.

Finish the rehearsal by testing a newsman unsubscribe at the restored sender, verifying the eligible audience and confirming that the interrupted campaign produced no duplicate submissions.
