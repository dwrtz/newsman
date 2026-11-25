# Operator identity, client grants and recipient proofs

One workspace can belong to one person or to a small allowlisted team. All active operators have the same application role. A grant issued to a remote client can carry a subset of that role. Recipients hold proofs for their own consent actions.

## Identity boundaries

| Entry point | Authority | Action |
| --- | --- | --- |
| Browser console and command API | Verified email-code session or Cloudflare Access identity on the allowlist | Read and change the workspace. A browser mutation includes a matching Origin and CSRF token. |
| `/mcp` | OAuth bearer grant bound to this installation’s MCP resource, operator and scopes | Discover and invoke authorized operator tools over HTTPS. |
| Authorization and consent | Authenticated operator session plus the operator’s explicit grant | Approve a registered client, exact callback, resource and scopes. |
| OAuth metadata, token and revocation endpoints | Issuer discovery or the relevant OAuth credential | Discover the issuer, exchange codes, renew and revoke credentials. Protocol endpoints return machine-readable challenges and errors. |
| Hosted recipient pages and `/reader/mcp` | Rate-limited signup or purpose-bound proof. Email-code mode also requires a resource-specific operator grant for reader MCP | Request confirmation, confirm or unsubscribe. |

## Configure the allowlist

In email-code mode, the deployment supplies the exact approved addresses. Send an eight-digit code only to an approved inbox and return the same acknowledgement for an unknown address. Bind the code to its requesting browser, expire it after ten minutes and invalidate it after five unsuccessful attempts or one successful use. Limit new challenges to one per minute and five per hour per address. Keep only a keyed digest of a live code. Use a Secure, HttpOnly, SameSite cookie and a server-side session with a twelve-hour lifetime. Signing out revokes the session. Check the current deployment allowlist when authenticating, admitting a roster member, authorizing commands, exchanging a code and refreshing a grant.

In email-code mode both MCP endpoints require OAuth, including discovery of the tools. A grant to the reader resource has read scope and authorizes reader tools with their existing proof requirements. Each grant names its exact resource. Web signup, confirmation and unsubscribe follow their own consent rules. The deployment may limit pilot signup and all mail to designated test recipients.

For an installation using Cloudflare Access, bootstrap exact verified operator addresses. An empty configuration creates no interactive administrator. Validate the Access application JWT with the configured team’s signing keys, issuer, expected audience and validity window. Bind an eligible verified email to a stable operator identity. An invitation alone cannot authorize commands. The Rust Worker verifies requests under its own authorization rules even if traffic reaches an alternate route. [Cloudflare Access JWT validation](https://developers.cloudflare.com/cloudflare-one/access-controls/applications/http-apps/authorization-cookie/validating-json/)

Manage membership using `operators_list`, `operators_add` and `operators_remove`. Protect the last bound active operator from concurrent removal. Removal invalidates the operator’s grants. Re-admitting a prior identity starts a fresh membership generation. During operation, check current D1 membership and grant status on every command. Persist active-membership and resource preconditions with the mutation or detect and retry concurrent changes. A storage outage must fail closed.

The local development identity is explicitly selected in an isolated local environment. It is unsuitable for a public deployment. The deployment guide identifies the variables for Access, public origin, bootstrap operator and client registration. Populate credential bindings using the hosting account’s secret store.

## Delegate a remote client

The Rust application is both the protected-resource implementation and its OAuth issuer. An unauthenticated operator request receives HTTP 401 with a Bearer challenge pointing to the RFC 9728 protected-resource metadata. That metadata identifies the issuer, whose authorization server metadata describes the authorization, token and revocation endpoints.

Use authorization code with PKCE S256. Bind the authorization transaction, single-use code and resulting grant to the verified user, registered client, permitted redirect, exact MCP resource and selected scopes. Store server-side token lookups as digests in D1. Keep raw bearer tokens, codes and refresh tokens out of logs and operator listings. A returned token has a finite validity period. When the operator consents to `offline_access`, a client can refresh its grant for unattended use. Revoking the grant or removing the operator prevents further authorized work. [MCP authorization](https://modelcontextprotocol.io/specification/2025-11-25/basic/authorization) · [Protected-resource metadata](https://www.rfc-editor.org/rfc/rfc9728)

A publisher can pre-register a public client or trust its HTTPS Client ID Metadata Document (CIMD). For CIMD, match the configured client ID, the document's client ID and an exact callback permitted by both the publisher and the document. Fetch only configured documents, without following redirects. The client and issuer must both support the public token authentication method `none`. The OAuth issuer advertises CIMD when configured and returns its exact `iss` on successful and error callbacks. Published metadata includes the `offline_access` scope for explicitly renewable grants. [MCP authorization](https://modelcontextprotocol.io/specification/2025-11-25/basic/authorization) · [OpenAI authentication](https://developers.openai.com/plugins/build/auth)

The four application scopes are `read`, `write`, `send` and `admin`. Together they permit the full web workflow. A grant for reporting needs only its required scopes. A title, cursor, file handle or job ID cannot enlarge the grant’s authority. Check the underlying object’s required access on every transfer step. A valid token with insufficient scope gets HTTP 403. An expired, invalid or wrong-resource token gets 401. Use `grants_list` and `grants_revoke` for inspection and removal through the web console or MCP.

## Transport and consent checks

Implement stateless Streamable HTTP with JSON-RPC 2.0, `initialize`, `notifications/initialized`, `ping`, `tools/list` and `tools/call`. Negotiate the requested supported revision among `2025-11-25`, `2025-06-18` and `2025-03-26`. The last revision supports bounded batches, text tool results and requests without a version header. Later revisions take one message per POST and support structured output. Notifications receive HTTP 202 and a standalone stream request can receive HTTP 405. Validate the Origin and any supplied protocol header. [MCP transport](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports)

For signup and resend, rate limit requests and return the same generic result for an unknown, pending, active or suppressed address. Confirmation and unsubscribe use distinct, purpose-bound proofs. A GET can safely display a landing page. A deliberate POST changes consent. Mailbox providers can invoke the signed one-click POST without cookies, login or interactive confirmation. [RFC 8058](https://www.rfc-editor.org/rfc/rfc8058)
