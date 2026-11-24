# Deploy one installation

Run the build and test commands in the [README](../README.md) first. A browser and an MCP client operate the same Rust Worker. One qualified operator address is enough to bootstrap it.

## Cloudflare

1. Sign into the Cloudflare CLI with an account that can deploy Workers and create D1, R2 and Queues resources. Enable R2 for the account. Choose the name in `NEWSMAN_WORKER_NAME` (default `newsman`) and an HTTPS operator hostname. The hostname can be a custom domain.
2. Create a D1 database, a private R2 bucket and the send and dead-letter queues in that account. Defaults are database `newsman`, bucket `newsman-files`, and queues `newsman-send` and `newsman-send-dead`. Use `NEWSMAN_DB`, `NEWSMAN_BUCKET` and `NEWSMAN_QUEUE` for alternate names. Export `NEWSMAN_D1_ID` for the created database. The dead-letter queue uses the selected queue name plus `-dead`.
3. Apply every SQL file in `migrations/` in filename order. This release's migrations can be repeated. The following uses Cloudflare's authenticated D1 API via its CLI.

   ```sh
   for sql_file in migrations/*.sql; do
     npx cf d1 query "$NEWSMAN_D1_ID" --sql "$(cat "$sql_file")" || break
   done
   ```

4. Add a Cloudflare Access application for the operator web/API and the OAuth consent route on the canonical domain. Allow the intended operator identities through Access. Configure specific path policies so the MCP, discovery, token, reader, image and signed SNS endpoints can reach the Worker with their own protocol credentials. The Worker also verifies the Access JWT issuer, audience and bound operator independently. [Operator access](access.md)
5. Supply the deployment variables below to the environment running `cf`. Keep secret values in Worker secrets. Use separate resources and secrets for a preview deployment. The local-development bypass is absent from the deployment configuration.

| Setting | Use |
| --- | --- |
| `PUBLIC_ORIGIN` | Exact HTTPS origin of the Worker serving MCP, login and OAuth. |
| `ACCESS_TEAM_DOMAIN`, `ACCESS_AUD` | Cloudflare Access team domain and application audience. |
| `BOOTSTRAP_OPERATORS` | Comma-separated verified email addresses admitted on first setup. Manage later membership from the console or MCP. |
| `OAUTH_CLIENTS` | JSON object mapping each approved public client ID to its exact redirect URIs. |
| `NEWSMAN_SECRET`, `READER_PROOF_SECRET` | Separate high-entropy Worker secrets of at least 32 characters. Keep them with the installation backup. The latter authenticates reader links and suppression identifiers. |
| `PUBLICATION_NAME`, `PUBLICATION_ORIGIN`, `FROM_ADDRESS`, `REPLY_TO` | Optional initial workspace, reader origin and sending defaults. The reader origin should route signup, confirmation, unsubscribe, media and redirects to newsman. Configure the postal footer in Settings. |
| `TIME_ZONE`, `ARCHIVE_LAG_DAYS`, `ARCHIVE_FEED_URL` | Optional presentation and archive defaults. A message can be sent without supplying a public page, feed or release date. |
| `AWS_REGION`, `SES_CONFIGURATION_SET`, `SNS_TOPIC_ARN` | Region, sending configuration set and exact feedback topic for this installation. |
| `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` | Worker secrets for the least-privilege AWS identity used to submit mail, read sending quota and confirm the SNS subscription. For temporary credentials set `NEWSMAN_AWS_TEMPORARY=yes`, configure `AWS_SESSION_TOKEN`, and rotate the temporary credential set together. |

Use the Workers dashboard’s Variables and Secrets settings or the CLI's Worker secret commands to install the production secrets. Verify resource names, routes and asset contents with `npm run check:deploy`. Run `npx cf deploy` from the configured build environment to publish your own instance. Bind the domain chosen in `PUBLIC_ORIGIN`. Cloudflare's scheduled trigger wakes the delivery/import worker each minute; it imposes no schedule on publication.

## Amazon SES

Verify the From domain in the configured region and publish its DKIM records. With a custom MAIL FROM domain, also publish the MX/SPF records supplied by AWS. Configure aligned DMARC and use a monitored Reply-To mailbox. Request production access in that region before sending to a general audience. The SES sandbox can be used with verified recipients and the mailbox simulator. [SES production access](https://docs.aws.amazon.com/ses/latest/dg/request-production-access.html)

Configure an SES event destination on the named configuration set using an SNS Standard topic. Permit SES to publish from the intended account/configuration set. Subscribe `https://YOUR-HOST/webhooks/ses` over HTTPS with raw message delivery disabled and SNS signature version 2. Enable send, delivery, bounce, complaint and reject events. The Worker verifies the exact topic, signature, certificate source and event identity. Ensure the sending identity can call `ses:SendEmail` and `ses:GetAccount` and can confirm the intended SNS subscription. Enable SES account-level suppression for hard bounces and complaints. [SES event publishing](https://docs.aws.amazon.com/ses/latest/dg/monitor-using-event-publishing.html) · [SNS message signing](https://docs.aws.amazon.com/sns/latest/dg/sns-verify-signature-of-message.html)

In Settings set the sender, postal footer, test recipients and a monthly send budget appropriate for your account. Check the delivery status, confirm a controlled signup and opt-out, then send a proof to a verified inbox and check its headers and feedback receipt. Enable dispatch when ready. Ambiguous provider submissions remain on hold for reconciliation. [Delivery and recovery](../spec/delivery.md)

## Data and recovery

Keep D1 backups, private R2 data, and keys in the installation owner's account. Apply migrations before enabling a new Worker version. After restoring an older database, keep dispatch paused while reconciling provider receipts, current suppressions and jobs since the backup. Test a restore using separate resources and the local capture sink or a controlled mailbox. Reported SES charges concern sending volume; Worker, queue, R2, D1 and SNS usage have their own meters.
