# Operate newsman over HTTP MCP

A client with the operator’s full delegated authority can do the whole job over HTTPS. The same Rust command updates a message whether an operator clicks Save in the console or an agent calls `issue_update`. A client can retrieve content, digests, receipts, reports and file bytes as structured results. Screen automation is unnecessary.

## One operation registry

[operations.json](operations.json) is the versioned inventory of names, scopes, views, inputs and outputs. The console uses the same inventory as MCP. The operator surface currently contains 59 tools and the separately exposed recipient surface contains five. Operators reach the HTTP adapter through `POST /api/ops/{name}` and remote clients use `POST /mcp`. Retain existing tool names for compatibility. On the wire, an “issue” is a message draft and a “campaign” is a committed delivery.

| Goal | Example calls in order |
| --- | --- |
| Compose a standalone message | `issue_create`, `issue_update`, `issue_render`, `send_test` |
| Import material and send | `file_begin`, `file_write`, `file_finish`, `issue_import`, `segment_preview`, `send_prepare`, `send_commit` |
| Follow a committed send | `campaign_get`, `attempts_list`, `report_get`. Use `send_cancel` to stop remaining submissions |
| Recover an interrupted task | `jobs_list`, `job_get`, `job_retry` or `attempt_resolve`, according to the recorded outcome |
| Export private records | `export_create`, `job_get`, successive `file_read` calls, then verify the result digest |
| Administer access | `operators_add`, `operators_list`, `grants_list`, `grants_revoke`, `operators_remove` |
| Manage one recipient’s consent | `reader_info`, `reader_subscribe`, `reader_resend`, `reader_confirm`, `reader_unsubscribe` on `/reader/mcp` |

## Connect

Configure the deployment’s canonical HTTPS origin. Connect the client to its `/mcp` endpoint, authorize the requested scopes, initialize, and discover tools with `tools/list`. The server supports MCP version `2025-11-25` with JSON-RPC 2.0 over Streamable HTTP. Requests include the negotiated protocol header and allow JSON and event-stream responses. At the operator endpoint a signed-in bearer grant is required. A missing or invalid credential receives an HTTP challenge carrying the protected-resource metadata URL. The reader endpoint exposes public signup and narrowly scoped actions on proofs received by email. [Authorization and transport](access.md)

## Try a send from an MCP client

The following JSON-RPC messages illustrate tool calls after initialization. Use IDs returned by the running installation. Creation, update and send commands use a caller-provided idempotency key.

```json
{"jsonrpc":"2.0","id":21,"method":"tools/call","params":{"name":"send_prepare","arguments":{"key":"prepare-release-003","issue_id":"message-003","issue_version":2,"audience":{"statuses":["active"]}}}}
```

Omitting a future send time requests dispatch on commit. Inspect the returned subject, sender, counts, costs and digest before committing. Pass the exact plan ID and digest to `send_commit` with a new command key. Use the resulting campaign ID to read progress.

```json
{"jsonrpc":"2.0","id":22,"method":"tools/call","params":{"name":"campaign_get","arguments":{"id":"campaign-003"}}}
```

A successful tool result provides the structured output declared by the registry and a text representation for clients that consume text. Clients can list and call tools without loading the console. Per-tool schemas resolve shared definitions and have object roots.

## Cross-interface consistency

- Authorize a call, validate its arguments and resolve resource-level permissions before returning data, including on retries and file reads. A grant for one operation does not imply permission for the records of a more sensitive job.
- An operator command carries a `key`. Persist its input fingerprint, receipt and domain mutation at the same durable boundary. Repeating the same command by the same principal with the same key and input returns its original result. A changed input under that key conflicts. Mutations of existing objects use the recorded version when the schema requires one.
- List results are paged using opaque cursors. Long-running work returns an inspectable job. File upload and download use bounded chunks, offsets, content digests and a final byte count. Clients can finish a private transfer and verify the object entirely over MCP.
- Both surfaces distinguish invalid input, denied access, unavailable storage, stale revisions, expired plans, throttling and ambiguous delivery. Return actionable execution errors. Unknown JSON-RPC methods and malformed messages use protocol errors. Signup and resend share the same generic acknowledgement across recipient interfaces.
- A client with a restricted grant can discover only its available capabilities or receive explicit permission errors. Revocation applies to already connected clients on the next operation. Local interface details such as opening a modal and the OAuth login and consent exchange use their respective presentation or identity protocols.

## Parity checks

Resolve the entire web operation inventory and compare it with the MCP tools accessible to the same actor. Exercise the HTTP endpoint using a separate client against the running Worker. Compare the two adapters’ validated inputs, result schemas, state effects and errors. A browser-only mutation, export that returns unreachable bytes, report with different filters or missing authorization check fails the release. Cross-channel retries must not create duplicate campaigns, recipient submissions, membership changes or import jobs. [Acceptance scenarios](acceptance.md)
