# newsman

newsman is a self-hosted alternative to beehiiv for sending email to people who have opted in. A museum can announce an exhibit, a software team can send release notes, and a writer can publish a newsletter. The operator chooses the purpose, audience and timing of each message. A single, email-only announcement is a complete use case.

**The HTTP MCP server is a full publishing interface.** An authorized client can compose, upload an image, choose an audience, test a message, inspect its cost, commit a send, read delivery receipts and export the results. The Rust server runs the same operations for the browser and MCP. An operator can begin a task in one client and finish it in another. [MCP contract](parity.md) · [Operation schemas](operations.json)

Run the Rust Worker and data stores in the operator’s Cloudflare account, using the operator’s Amazon SES identity for delivery. The reference implementation is available under the MIT license. [Source and installation guide](https://github.com/dwrtz/newsman)

## R1. Install one independent workspace

One installation owns its messages, audience, operators, credentials and records. Bootstrap a single allowlisted operator or a small team of named operators. Collect a display name, sender identity, monitored reply address, footer, time zone and public HTTPS origin. newsman can serve its own recipient signup and consent pages. Operators can also link to those pages from an existing site. An installation can begin empty.

**Accept when** the operator can create and preview a draft in a clean installation, see which delivery prerequisites remain, and connect a remote MCP client. An empty allowlist closes operator access.

## R2. Provide every application action over HTTP MCP

The public URLs of the two MCP surfaces are the installation origin plus `/mcp` for operators and `/reader/mcp` for recipients. The registry defines a named tool for every web action, including reports, private file transfers, revisions, grant management and failure recovery. Use a shared command and query implementation. A visual control and its tool must apply the same authorization, validation and side effects. An operator can delegate fewer scopes to an individual client.

**Accept when** a full-scope client can perform every console operation with structured inputs and outputs, including retrieving the bytes of a generated export. The release test detects a web operation missing from MCP. [Parity tests](acceptance.md)

## R3. Record consent and respect opt-outs

The hosted signup form accepts an email address and optional acquisition tags. Use a generic check-inbox response. A confirmation email provides a purpose-bound proof. Opening the landing page displays the confirmation step. The recipient’s explicit action records consent once. Support confirmation resends and idempotent opt-outs by form, recipient MCP tool or one-click email action. Preserve the distinction between a voluntary unsubscribe, a hard bounce and a complaint. Require fresh consent for voluntary re-subscription and retain protective holds.

**Accept when** duplicate submissions and a link scanner’s GET create zero extra confirmations, and an opt-out blocks a queued delivery that has yet to reach the provider.

## R4. Manage the audience and its data

Search recipients, inspect consent history, manage tags and save segments. Show audience counts and suppression exclusions before committing a send. Export records in a portable format. Provide audited suppression and erasure with a minimal keyed suppression record for respecting previous opt-outs. Stage bulk input, inspect diagnostics and reconcile the eligible set before committing an import. An operator can start with new signups or import from a prior provider.

**Accept when** an imported active row cannot override a complaint. A segment preview and its exported results identify the same eligible recipients at the same cut. [Import and cutover](migration.md)

## R5. Create a message

Save versions of the title, subject, preheader, HTML and plain text. Work from a blank draft, a template or a supplied source bundle. Include optional accessible images and links. Render previews from the same validated content used for the send. The stable tool names beginning with `issue_` apply to any message. An operator may optionally attach a canonical URL and choose a time for a web release. The default workflow ends in the recipient’s inbox.

**Accept when** a one-off message can be composed, rendered and sent using only the hosted app and an email address. An operator can restore an old revision, preview the result and distinguish the draft from a prior approved version.

## R6. Choose when and to whom to send

Support sending immediately and scheduling a particular message for an operator-selected instant. Test sends go to configured test recipients. A prepare operation returns the content digest, audience selection, counts, sender identity, intended send time and estimated base cost. A separate commit accepts that exact plan. Recheck every recipient’s eligibility before submission. A cancellation fences remaining work. The console and MCP expose uncertainty about any request already sent to SES.

**Accept when** committing a reviewed plan through the web and replaying it through MCP produces one campaign. Editing its source cannot change the frozen content. Two messages can be sent at unrelated times without a configured series or subscription interval.

## R7. Audit delivery and engagement

Distinguish provider acceptance, destination-server delivery, bounce, complaint, opt-out and uncertain submission. A duplicate notification must have one effect. Provide filtered reports with units, observation windows and denominators. Clearly label optional click measurement, imported provider statistics and unavailable data. Keep attribution optional and apply the installation’s reporting time zone. Tie delivery and operator actions back to the relevant immutable revision.

**Accept when** repeated or out-of-order delivery feedback leaves campaign totals and suppression consistent. An authorized client can page reports and audit records through either interface. [Delivery contract](delivery.md)

## R8. Offer an optional public handoff

When the operator chooses a web copy, export an archive bundle and preserve supplied identifiers and release metadata. The owner of that website controls its publication. A message can precede, coincide with or follow that release. Verification records what a request to the configured destination actually observes. Links presented to recipients must resolve at the time of use.

**Accept when** a message with no canonical URL, feed or release date needs no archive operations to send. An opt-in handoff can be downloaded and later verified without creating another delivery.

## R9. Operate and recover

Show sender readiness, queue health, provider limits and available recovery actions. Use durable job and delivery records to resume work after interruptions. Separate local mail capture and production sending. Keep credential material, confirmation proofs and addresses out of operational logs. A restore must carry forward recent suppressions and reconcile attempts with uncertain outcomes.

**Accept when** the same message is processed through a duplicate wake-up, a simulated provider timeout and an operator recovery, with one delivery or an explicit unresolved attempt for every eligible recipient.

## Screenshots

The gallery shows a running instance with example data. The overview presents workspace activity, the composer edits an individual message, and the MCP connection screen gives a client the endpoint and access model. [Overview](shots/overview.png) · [Composer](shots/compose.png) · [HTTP MCP connection](shots/mcp.png)
