# Deploy and deliver

## Run in the operator’s accounts

| Resource | Responsibility |
| --- | --- |
| Rust Worker compiled to WebAssembly, with static browser assets | Serve the console, remote MCP server, signup and consent pages, OAuth, dispatch and delivery feedback. |
| Cloudflare D1 | Keep operator and client grants, consent, suppressions, revisions, dispatch plans, recipient attempts, receipts, reports and audit events. |
| Private Cloudflare R2 | Store immutable uploaded media, import files and downloadable results. Check authority on access and expire staged objects according to the installation’s data policy. |
| Cron trigger and optional Queue wake-ups | Drain due work. Durable records in D1 allow another invocation to find it after an interruption. A wake-up contains work identifiers. |
| Amazon SES v2 and an SNS Standard topic | Submit messages using a regional verified sender. Deliver signed feedback to `/webhooks/ses`. |

Deploy each environment separately. The local Worker uses an isolated capture transport, storage and identities. Configure the production origin, allowlisted operators, OAuth clients, verified From domain, monitored Reply-To, postal footer, SES region, configuration set, credentials, feedback topic and sending limits. Previewing a draft requires no AWS access. [Workers bindings](https://developers.cloudflare.com/workers/runtime-apis/bindings/) · [SES production access](https://docs.aws.amazon.com/ses/latest/dg/request-production-access.html)

A deployment can restrict recipient addresses at signup, immediately before SES submission and in the AWS sending role while an existing provider handles the newsletter. Campaign pause still allows confirmations and approved proofs. A proof to an eligible, explicitly selected seed subscriber carries working one-click headers and a visible opt-out for that subscriber. An ordinary preview to another approved test address carries preview content.

An AWS workload identity can exchange a five-minute signed assertion for temporary role credentials using STS. Publish its issuer metadata and public key at the canonical installation origin. Limit the role trust to the exact issuer, audience and subject. Restrict the role to the sender, configuration set, regional quota and feedback subscription. Keep signing material in Worker secrets.

## Freeze the message and audience

An operator creates a standalone message or imports a versioned source bundle. Render safe email HTML and a plain-text alternative and include a visible unsubscribe control. Freeze sender, subject, preheader, body, asset references, audience selection, version and digest at commitment. A registered command key and unique plan prevent duplicate campaigns even across adapters. Snapshot the eligible recipients at that cut. A later signup is eligible for a future send. Any subsequent suppression can still stop an outstanding delivery.

Publishers who attach a web URL can offer a read-online link when that location is available. Make an optional image accessible to the intended email client without exposing private draft or import objects. The email workflow uses no pre-existing website, issue frequency, public archive or feed.

## Submit with a durable boundary

1. Stage one recipient attempt for each unique campaign and eligible recipient. Before calling SES, atomically check campaign state, consent, suppression, current limits and the attempt’s state. Persist that the attempt is crossing the provider boundary. A later cancellation stops other queued work.
2. Use a SigV4-signed regional SES v2 `SendEmail` request. Include the configured sender, monitored Reply-To, one recipient, HTML and text alternatives, the SES configuration set and opaque correlation tags. Keep automatic network retries off for this submission. Record a returned SES message ID as provider acceptance.
3. Record a definite validation rejection, back off from definite throttling and retain an uncertain result when a response is lost or ambiguous. Duplicate worker invocations cannot make a new submission for an already claimed attempt. A matching signed SES event may independently establish acceptance. The operator can view unresolved attempts and record supporting evidence, permanently skip an attempt, or authorize another submission after establishing that the first did not occur.
4. At dispatch, apply account sending rates, rolling quotas and the installation’s local budget. Include confirmations and tests. A pause leaves drafts, recipient controls and feedback available. A restore starts with dispatch paused and reconciles accepted and uncertain attempts, new consent and suppressions before resumption. [SES quotas](https://docs.aws.amazon.com/ses/latest/dg/manage-sending-quotas.html) · [SES error handling](https://docs.aws.amazon.com/ses/latest/dg/troubleshoot-error-messages.html)

## Authenticated feedback

Enable DKIM and domain alignment for the verified regional sender. For a custom MAIL FROM configure its MX and SPF records as instructed by SES. The message carries a visible unsubscribe URL in both alternatives and the RFC 8058 `List-Unsubscribe` and `List-Unsubscribe-Post` headers. A qualifying DKIM signature must cover these headers. Verify an actual controlled seed message and the one-click action before general delivery. [SES DKIM](https://docs.aws.amazon.com/ses/latest/dg/send-email-authentication-dkim.html) · [SES one-click unsubscribe](https://aws.amazon.com/blogs/messaging-and-targeting/using-one-click-unsubscribe-with-amazon-ses/)

Verify an SNS envelope’s AWS topic, message type, timestamp, permitted certificate location and signature before applying any delivery state. Authenticate subscription confirmations. Store the SNS message ID and normalize the semantic delivery event for deduplication. Preserve provider event times even when notifications arrive out of order. Apply an authenticated hard bounce or complaint to the corresponding reader. An older delivery event cannot reverse that suppression. [SNS signature verification](https://docs.aws.amazon.com/sns/latest/dg/sns-verify-signature-of-message.html) · [SES event payloads](https://docs.aws.amazon.com/ses/latest/dg/event-publishing-retrieving-sns-contents.html)

Commit each feedback receipt, delivery change and suppression together. When another writer commits first, retry from the current database state and check the SNS message ID again. Acknowledge a duplicate that already committed. If contention persists, return HTTP 503 so SNS can redeliver the notification. [SNS HTTP delivery retries](https://docs.aws.amazon.com/sns/latest/dg/sns-message-delivery-retries.html)

## Costs and recovery

Base SES expenditure scales with the number of outbound messages, including confirmations and tests. For illustration, 4,000 messages at the reference à-la-carte price of $0.10 per thousand have a base send charge of $0.40. A one-off send to 40,000 eligible recipients would have a $4.00 base charge at that rate. These examples assume neither an interval nor a list size. Factor in applicable data and optional service charges, SNS notifications, Worker requests, D1 operations, queue usage and R2. Use current regional prices and account allowances for an installation-specific estimate. [SES pricing](https://aws.amazon.com/ses/pricing/) · [SNS pricing](https://aws.amazon.com/sns/pricing/) · [Workers pricing](https://developers.cloudflare.com/workers/platform/pricing/) · [D1 pricing](https://developers.cloudflare.com/d1/platform/pricing/) · [Queues pricing](https://developers.cloudflare.com/queues/platform/pricing/) · [R2 pricing](https://developers.cloudflare.com/r2/pricing/)

Retain backups and test restoration of D1, private files and signing configuration in isolation. D1 Time Travel covers the database. Reconcile protected files and post-backup consent, suppression and dispatch records separately before enabling production delivery. [D1 Time Travel](https://developers.cloudflare.com/d1/reference/time-travel/)

### Confirmation reminders

With `CONFIRMATION_REMINDER_ENABLED=true`, a new pending signup schedules one reminder for 48 hours after the initial request. The next available maintenance tick issues a fresh 24-hour confirmation link and queues the email in the same transaction that marks the reminder sent. Manual requests do not postpone the reminder. Confirmed readers, opt-outs, erased and suppressed addresses, and imports without a reader-initiated request receive no automatic reminder. The delivery worker rechecks the current challenge before submission.
