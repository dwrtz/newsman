# Build and acceptance

Use the MIT-licensed Rust reference implementation for code, local setup and deployment configuration. A conforming installation is a Cloudflare Worker compiled from Rust to WebAssembly with web assets, D1 and private R2. Amazon SES v2 provides email delivery and signed SNS notifications supply feedback. Configure access and credentials in the publisher’s own accounts. An isolated local instance uses a mail capture transport. [Source and setup](https://github.com/dwrtz/newsman)

The pull choices configure the initial display name (`publication_name`), deployment HTTPS origin (`public_origin`), operator time zone (`time_zone`), optional archive handoff delay (`archive_lag_days`), and regional SES identity (`ses_region`). A delay of zero adds no wait. The send time is chosen separately for every actual send. The operator can complete all workflows with the hosted console, recipient pages and MCP.

## Start and secure the workspace (R1, R2)

Build with the pinned Rust dependency lockfile and Wasm target. Use the generated Worker artifact in an isolated Cloudflare-compatible runtime. Provision the initial operator explicitly and close operator access when none is configured. Exercise both the web and bearer-token paths. Try an unauthorized identity, an expired credential, a mismatched resource, an incorrect redirect URI and a revoked grant. Confirm all four scopes permit the full operator tool set while a read-only grant cannot send. Verify that removing a bound operator invalidates their existing grants.

## Run a one-off send (R2, R3, R4, R5, R6)

1. Start an empty workspace and create a general announcement. Leave the source, archive and feed unset. Use a non-newsletter fixture such as a one-time registration notice and choose the test environment’s configured reply address.
2. Register one synthetic recipient through the hosted form and another through the public reader endpoint. A confirmation-link GET or repeated signup must not add an active recipient. A valid confirmation POST followed by a retry produces one activation. Suppressed and pending recipients stay out of the eligible count.
3. In one interface save a subject, content, plain text and preheader. Open the same revision from the other. Review exact rendered HTML and text. Test a transfer by staging an image or source file through one interface, finishing through the other and comparing the returned hash.
4. Send a proof to a permitted test destination. Prepare a send, inspect its audience, cost and content digest, then commit the exact returned plan. Do the same with a future operator-specified instant. An empty release calendar or external website setting cannot prevent either operation.
5. Repeat the send command in the other interface using the same key. Inspect the campaign and attempt records. A cross-interface retry produces one durable campaign and one submission per eligible recipient. A changed payload with a reused key conflicts.

## Exercise every interface (R2, R4, R7, R8)

Connect a standard remote MCP client to the running Worker and perform discovery. Compare the operator tool names against the full console operation registry and compare the reader surface independently. Check the schemas and spot-check values transferred between interfaces. Every operator command must have a shared authorization and idempotency boundary. Create a test-only web operation with no MCP registration and verify the parity check fails. Retrieve a complete exported file through successive `file_read` calls and compare bytes. [Parity contract](parity.md)

Use one optional source bundle with an explicitly supplied canonical URL and release time. Confirm its archive handoff preserves the supplied identifiers and that a content correction does not enqueue a campaign. An unrelated message with no source metadata can immediately return to normal email sending.

## Delivery and recovery (R3, R6, R7, R9)

| Scenario | Expected outcome |
| --- | --- |
| Concurrent commits, retried queue notification or overlapping schedule ticks | One campaign receipt and one persisted submission fence for each unique eligible recipient. |
| SES accepts a request but the response is lost | An uncertain attempt remains held until signed provider evidence or deliberate operator resolution. |
| Opt-out after scheduling, before provider submission | The final send boundary excludes that recipient. |
| Opt-out after the provider boundary | The prior submission may finish. Subsequent submissions are blocked. |
| Duplicate or reordered SNS events | The report is stable and an old delivery cannot reverse a complaint or permanent bounce. |
| Invalid signature or unexpected topic | Delivery and consent state are unchanged. |
| Import replay or rollback | The reviewed eligible set and all new opt-outs survive. See the seven-identity fixture in [migration.md](migration.md). |

Capture actual signup, delivery and report receipts from controlled tests. Inspect one-click headers, validate the endpoint with a controlled seed inbox, and rehearse a restore in isolation ahead of production sending. The operator can see uncertain attempts, inspect the relevant evidence and choose the resulting action in either interface.

## Reference screens

Generate gallery captures from the running console with only synthetic content. Show the overview, the message composer and the page a publisher uses to connect an MCP client. Keep credentials, personal data and live subscriber records out of screenshots. [Overview](shots/overview.png) · [Composer](shots/compose.png) · [HTTP MCP](shots/mcp.png)
