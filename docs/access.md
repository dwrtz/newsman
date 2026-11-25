# Connect an operator or an MCP client

A newsman installation has one publication. Each address you add to its operator allowlist can run the entire publication. Readers can subscribe without an operator account.

## Sign in by email

Set `OPERATOR_AUTH_MODE=email`, `OPERATOR_EMAILS` to the exact comma-separated operator addresses, and `PUBLIC_ORIGIN` to the HTTPS Worker origin. Configure the session secret, SES sender and the permitted OAuth client callbacks. The login page sends a one-time code to an approved inbox. Enter it in the requesting browser. Accounts start from the deployment allowlist, and future roster and grant operations remain subject to that list. Restrict access to the settings used for deployment.

In this mode, a client connects to `/mcp` using the normal OAuth code flow. A client for `/reader/mcp` requests a separate grant with scope `read` and the full reader endpoint as its resource. The public web confirmation and unsubscribe pages still accept recipient proofs. The console offers sign-out for the current browser session.

## Configure Cloudflare Access

| Setting | Value |
| --- | --- |
| `PUBLIC_ORIGIN` | Canonical HTTPS origin of this Worker, for example `https://news.example`. Use the same spelling in the client resource URL. |
| `ACCESS_TEAM_DOMAIN` | The issuer of your Cloudflare Access team, for example `https://my-team.cloudflareaccess.com`. |
| `ACCESS_AUD` | The application audience tag from Access. Use the same Access application for the browser API and the OAuth consent page. |
| `BOOTSTRAP_OPERATORS` | Comma-separated, exact verified addresses for the first installation. At least one of these operators must sign in to claim a seat. Subsequent deploys preserve the roster stored in D1. |
| `NEWSMAN_SECRET` | A random Worker secret of at least 32 characters. Rotating it invalidates browser form tokens. |
| `OAUTH_CLIENTS` | JSON object mapping each public client ID to its exact registered callback URLs. For example, `{"local-editor":["http://127.0.0.1:9876/callback"],"publisher":["https://my-client.example/oauth/callback"]}`. Copy the callback from the client you use. |

Apply the Cloudflare Access login policy to the operator app, `/api/session`, `/api/registry`, `/api/ops/*`, and `/oauth/authorize`, including form submissions. Configure separately scoped Access applications or path policies as needed for your hostname. Public reader forms, `/reader/mcp`, `/mcp`, OAuth discovery, token exchange and revocation must reach the Worker without a browser login redirect. The resource server challenges unauthenticated MCP calls itself. Cloudflare Access should authenticate the humans you want to admit. The D1 roster remains authoritative on every operation.

The Worker validates Access's RS256 signature, issuer, audience and validity period. It binds the first verified sign-in to the roster entry and the stable Access subject. Add and remove operators in the console or with `operators_add` and `operators_remove`. A removed operator's old grant stays revoked if their address is added again. A sole bound operator can add and bind a replacement before removing their own seat.

### Recover a lost operator identity

An infrastructure owner who controls the Cloudflare account can restore access from D1. Verify the replacement address in the configured Access identity provider and take a D1 backup. With operator writes paused, insert one new `operator` record with a fresh `op_` ID, its exact normalized address, version `1`, and state `invited`. The record's `body.id` must equal its table `id`. Increment `newsman_state.revision` in the same D1 batch. Keep a recovery record in the infrastructure change log. Sign in with the replacement identity, confirm its active membership, then remove the lost identity through the console or MCP to revoke its connections.

## Connect ChatGPT or Grok Bot

Configure `OAUTH_CIMD_CLIENTS` with the HTTPS client document as the key and the exact approved redirect URIs as its array of values. Copy the current callbacks from each provider's published document. Newsman checks the document against that configuration before authorizing a grant. Static public clients in `OAUTH_CLIENTS` continue to work.

- In ChatGPT, add a custom remote MCP server at the installation's `/mcp` URL. Choose OAuth and Client ID Metadata Document (CIMD) with public-client PKCE. ChatGPT's stable client ID is `https://chatgpt.com/oauth/client.json`. The stable redirect for an issuer-identifying server is `https://chatgpt.com/connector_platform_oauth_redirect`. [ChatGPT setup](https://developers.openai.com/plugins/deploy/connect-chatgpt) · [ChatGPT client document](https://chatgpt.com/oauth/client.json)
- In Grok Bot, open **Connect apps**, choose **Add a custom connector**, select **Remote HTTPS**, and enter the same `/mcp` URL. Use the published client ID `https://cursor.com/oauth/mcp-client.json` for automatic OAuth discovery. For a client configured manually, enter the public ID pre-registered for that Grok Bot deployment. The hosted and desktop callbacks are distinct. [Grok Bot setup](https://docs.x.ai/grok-bot/computer-and-apps) · [Connector backend client document](https://cursor.com/oauth/mcp-client.json)

Sign in to Newsman with an approved operator email and review the requested scopes. Both hosted clients use the authorization code flow with S256. A client secret is unnecessary for this profile.

## Connect a client

1. Configure the remote Streamable HTTP server URL `https://news.example/mcp` and the pre-registered client ID and callback. The resource metadata is at `/.well-known/oauth-protected-resource/mcp`. The issuer publishes its authorization server metadata at `/.well-known/oauth-authorization-server`. The metadata endpoints work without cookies.
2. Start the OAuth authorization code flow. Include the canonical MCP URL as the `resource`, a fresh client state, the registered redirect URI and an S256 PKCE challenge. Select the scopes the task needs. `read` covers reporting and normal inspection, `write` covers content and audience work, `send` controls campaigns, and `admin` covers sensitive data and administration. Full web parity uses all four. Include `offline_access` for a renewable unattended connection.
3. Sign in through Access and approve the client. Exchange the returned short-lived code at `/oauth/token` using a URL-encoded POST with `grant_type=authorization_code`, `client_id`, `code`, `code_verifier`, the exact `redirect_uri`, and `resource`. Verify the returned state and issuer in the client. Call MCP with `Authorization: Bearer <access token>`. Each access token lasts up to 15 minutes.
4. For a renewable grant, POST `grant_type=refresh_token`, `client_id`, the current `refresh_token` and `resource` to the same token endpoint. Store each newly rotated refresh token before issuing another request. Serialize refreshes. Reusing a spent refresh token revokes the grant. A grant can last up to 30 days. Operators can revoke earlier under Connections or with `grants_revoke`. A client can revoke at `/oauth/revoke` using `client_id` and `token`.
5. Open the MCP session with `initialize`, complete it with `notifications/initialized`, then use `tools/list` and `tools/call`. The preferred protocol revision is `2025-11-25`. Clients can also negotiate `2025-06-18` or `2025-03-26`. Send both `application/json` and `text/event-stream` in Accept and send the negotiated `MCP-Protocol-Version` after initialization. The reader client uses `/reader/mcp` and receives only the five reader tools. Confirmation and unsubscribe each require the matching reader link proof.

The Rust Worker issues OAuth credentials and stores only their hashes, together with expiry and grant state, in the publication's D1. Revocation and operator removal take effect on the next authenticated operation or refresh. Protect the D1 database and its backups as publication data.

## Local development

With `NEWSMAN_LOCAL=yes`, a loopback request, and the production Access and AWS credential settings absent, development may use a simulated verified identity. The default is `operator@newsman.test`. Set `LOCAL_OPERATOR_EMAIL` to a consistent address when exercising a different local roster. On first setup the local identity seeds the roster if an explicit bootstrap list is absent. A fresh local installation supplies OAuth client `newsman-local` with callback `http://127.0.0.1:9876/callback` if no client configuration is present. The local client uses the same consent and PKCE exchange as a deployed client. Bind the local development listener to loopback.

[Cloudflare Access token validation](https://developers.cloudflare.com/cloudflare-one/access-controls/applications/http-apps/authorization-cookie/validating-json/) · [MCP authorization](https://modelcontextprotocol.io/specification/2025-11-25/basic/authorization) · [MCP transport](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports)
