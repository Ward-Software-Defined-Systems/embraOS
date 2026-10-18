# A Microsoft 365 shared mailbox for `http_request`

This runbook gives embraOS one mailbox and nothing else in a Microsoft 365
tenant: a Microsoft Entra application scoped with **Exchange Online RBAC
for Applications** to a single shared mailbox (`embraos@example.com` here),
and its access token stored in the OS with `/guardian secret`, where the
shipped `http_request` tool
([GUARDIAN-HTTP-REQUEST-EXAMPLE.md](./GUARDIAN-HTTP-REQUEST-EXAMPLE.md))
sends it on every request to Microsoft Graph and the intelligence never
sees it. Reading and sending mail is then a plain-language ask.

**Validated:** in use with embraOS since 2026-10-09: listing, reading and
sending through `http_request` with the token stored as below (Steps 1, 2,
4 and 5). Step 3's scope test, 2026-10-18: in scope for the shared mailbox,
out of scope for another.

## What the OS enforces, and what it trusts

- The token is stored host-side (`SystemConfig.guardian_secrets`). The
  request guard sets the `Authorization` header on a request whose host is
  `graph.microsoft.com` and on no other, a redirect to another host never
  carries it, the envelope never echoes it, and a tool cannot set the
  header itself (`/guardian secret` is the only way in). See
  [COMMAND-REFERENCE.md](../OPERATIONS/COMMAND-REFERENCE.md).
- Which mailboxes the token reaches is Exchange Online's decision, made by
  the RBAC scope below. The OS trusts the token for what it is; the scope
  is what keeps it to one mailbox. Step 0 is where that goes wrong.
- `graph.microsoft.com` is a public host. The egress allowlist
  (`/guardian egress allow`) is for private hosts only and is not needed
  here; the command accepts the name and nothing changes.

## Step 0: the Entra API permissions

In the **Microsoft Entra admin center** (`entra.microsoft.com`) →
**Identity** → **Applications** → **App registrations** → your app →
**API permissions**:

- If the app ever received tenant-wide admin consent to the Microsoft Graph
  **Application** permissions `Mail.Read`, `Mail.ReadWrite` or `Mail.Send`,
  remove those permissions or revoke the consent.
- Entra API permissions and Exchange Online RBAC for Applications are
  **additive**. An app that holds tenant-wide `Mail.Read` in Entra reaches
  every mailbox in the tenant, whatever Exchange scope is created later.
- With RBAC for Applications, Exchange Online itself grants `Application
  Mail.Read` and `Application Mail.Send` on the scoped mailbox alone. Tokens
  are still minted against `https://graph.microsoft.com/.default`.

## Step 1: the identifiers

1. **Tenant ID** (`$TenantId`): **Entra ID** → **Overview**.
2. **Application (client) ID** (`$AppId`) and **Enterprise application
   object ID** (`$SpObjectId`): **Entra ID** → **Enterprise applications**
   (not App registrations) → your application → **Overview**. The object ID
   on the App registrations blade is a different object and fails in
   Exchange Online.
3. **Client secret** (`$ClientSecret`): **Entra ID** → **App registrations**
   → your app → **Certificates & secrets** → **New client secret** → the
   secret's **Value**.

## Step 2: the Exchange Online scope (PowerShell)

With the `ExchangeOnlineManagement` module (`Install-Module
ExchangeOnlineManagement` once):

```powershell
# 1. Connect as an Exchange Administrator or Global Administrator
Import-Module ExchangeOnlineManagement
Connect-ExchangeOnline

# 2. The identifiers from Step 1 and the shared mailbox
$AppId         = "<YOUR-APPLICATION-CLIENT-ID>"
$SpObjectId    = "<YOUR-ENTERPRISE-APP-OBJECT-ID>"
$SharedMailbox = "embraos@example.com"

# 3. Register the Entra service principal inside Exchange Online
New-ServicePrincipal `
    -AppId $AppId `
    -ObjectId $SpObjectId `
    -DisplayName "embraOS-Graph-Mail-SP"

# 4. A management scope that matches that one mailbox
New-ManagementScope `
    -Name "embraOS-SharedMailbox-Only" `
    -RecipientRestrictionFilter "PrimarySmtpAddress -eq '$SharedMailbox'"

# 5. Application Mail.Read (or Mail.ReadWrite) and Application Mail.Send, in that scope
New-ManagementRoleAssignment `
    -Name "embraOS-MailRead-Scoped" `
    -App $AppId `
    -Role "Application Mail.Read" `
    -CustomResourceScope "embraOS-SharedMailbox-Only"

New-ManagementRoleAssignment `
    -Name "embraOS-MailSend-Scoped" `
    -App $AppId `
    -Role "Application Mail.Send" `
    -CustomResourceScope "embraOS-SharedMailbox-Only"
```

`Application Mail.ReadWrite` in place of `Application Mail.Read` lets the
intelligence mark messages read or unread, create drafts and move messages
between the mailbox's folders.

## Step 3: the scope, verified

Exchange Online answers whether the service principal is in scope for a
mailbox:

```powershell
# The shared mailbox: InScope True for both roles
Test-ServicePrincipalAuthorization -Identity $AppId -Resource $SharedMailbox | Format-List

# Any other mailbox: InScope False
Test-ServicePrincipalAuthorization -Identity $AppId -Resource "operator@example.com" | Format-List
```

A role assignment that `Test-ServicePrincipalAuthorization` reports as in
scope takes 5 to 30 minutes to reach Microsoft Graph.

## Step 4: a token, into the OS

A client-credentials token lasts about an hour (Entra's default). Mint one
when the mailbox is needed:

```powershell
$TenantId     = "<YOUR-TENANT-ID>"
$AppId        = "<YOUR-APPLICATION-CLIENT-ID>"
$ClientSecret = "<YOUR-CLIENT-SECRET>"

$TokenResp = Invoke-RestMethod -Method Post `
    -Uri "https://login.microsoftonline.com/$TenantId/oauth2/v2.0/token" `
    -Body @{
        client_id     = $AppId
        client_secret = $ClientSecret
        scope         = "https://graph.microsoft.com/.default"
        grant_type    = "client_credentials"
    }

$TokenResp.access_token
```

```bash
TENANT_ID="<YOUR-TENANT-ID>"
APP_ID="<YOUR-APPLICATION-CLIENT-ID>"
CLIENT_SECRET="<YOUR-CLIENT-SECRET>"

curl -sS -X POST "https://login.microsoftonline.com/${TENANT_ID}/oauth2/v2.0/token" \
  -H "Content-Type: application/x-www-form-urlencoded" \
  --data-urlencode "client_id=${APP_ID}" \
  --data-urlencode "client_secret=${CLIENT_SECRET}" \
  --data-urlencode "scope=https://graph.microsoft.com/.default" \
  --data-urlencode "grant_type=client_credentials" | jq -r '.access_token'
```

Then, in the embraOS console:

```text
/guardian secret graph.microsoft.com Authorization "Bearer <PASTE_ACCESS_TOKEN>"
```

The value is stored in STATE and shown by no surface. When the token
expires, Graph answers 401 and the intelligence reports it; store a new
one with the same command, which replaces the old value.

## Step 5: what the requests look like

Nothing here has to be taught to the intelligence. It knows Microsoft
Graph, and a plain-language ask ("list the unread mail in the shared
mailbox", "reply to the message from the supplier") is enough: it composes
the request itself and sends it with `guardian_call` (`action: "invoke"`,
`tool: "http_request"`). The shapes below are what such calls look like,
for the operator who wants to read the Tools tab or try one by hand.
`<mailbox>` is the shared mailbox's address, `<id>` a message id from a
listing.

1. **The inbox, ten newest** (`GET`):

   ```json
   { "method": "GET",
     "url": "https://graph.microsoft.com/v1.0/users/<mailbox>/mailFolders/inbox/messages?$top=10&$select=id,subject,from,receivedDateTime,isRead,bodyPreview" }
   ```

2. **Unread only**: the same with `&$filter=isRead eq false` on the URL.

3. **One message as plain text** (`GET`; the `Prefer` header asks Graph
   for text in place of HTML):

   ```json
   { "method": "GET",
     "url": "https://graph.microsoft.com/v1.0/users/<mailbox>/messages/<id>?$select=id,subject,from,toRecipients,ccRecipients,receivedDateTime,body",
     "headers": { "Prefer": "outlook.body-content-type=\"text\"" } }
   ```

4. **Send** (`POST`; `json` sets `Content-Type: application/json`, so
   neither the header nor an escaped body string is needed; Graph answers
   `202` with an empty body):

   ```json
   { "method": "POST",
     "url": "https://graph.microsoft.com/v1.0/users/<mailbox>/sendMail",
     "json": { "message": { "subject": "Subject line",
                            "body": { "contentType": "Text", "content": "The text" },
                            "toRecipients": [ { "emailAddress": { "address": "recipient@example.com" } } ] },
               "saveToSentItems": true } }
   ```

5. **Reply** (`POST`, in the thread; `202`, empty):

   ```json
   { "method": "POST",
     "url": "https://graph.microsoft.com/v1.0/users/<mailbox>/messages/<id>/reply",
     "json": { "comment": "The reply's text" } }
   ```

6. **Mark read** (`PATCH`; needs `Application Mail.ReadWrite` from Step 2):

   ```json
   { "method": "PATCH",
     "url": "https://graph.microsoft.com/v1.0/users/<mailbox>/messages/<id>",
     "json": { "isRead": true } }
   ```

What comes back is the tool's envelope: `status`, `url`, `headers` and the
body as Graph sent it (JSON, so `as: "auto"` leaves it as it is), scanned
for prompt injection the way every body is: `injection_suspected` is false
unless the scan matched, and `injection_signals` names each match. A
message body is untrusted text like any page; the intelligence reads it as
content, not as instructions.

## Try it

```text
/guardian status http_request     # → ready
# then ask in plain language, e.g.:
#   "list the unread mail in the shared mailbox and summarize it"
#   "read the message from <sender> in full"
#   "reply to it: we confirm the appointment on Tuesday"
```
