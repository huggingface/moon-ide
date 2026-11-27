# ADR 0130: A headless IDE signs in to Hugging Face through its bridge

Extends [ADR 0059](0059-headless-enrolled-ide.md) and
[ADR 0087](0087-companion-end-to-end-encryption.md).

## Context

A headless `moon-remote` had to be signed in over SSH
(`moon-remote login`, device flow). Doing it from the phone with the
device flow works but means reading a code off one screen and typing
it into another. Hugging Face accepts OAuth clients identified by a
client metadata document (CIMD): any HTTPS URL serving the client's
name, logo and redirect URIs, with no registration.

## Decision

- **The bridge's public origin is the OAuth client.** It serves
  `/oauth/client.json` (name, logo, redirect URI) derived from its
  advertised URL, and is the redirect target, `/oauth/callback`.
- **The IDE holds the PKCE verifier and does the exchange.** The phone
  asks the IDE to start; the IDE returns an authorize link and
  watches for the result. Hugging Face redirects to the bridge, which
  parks the code under its `state` for a few minutes; the IDE collects
  it from `/oauth/code` and exchanges it. The token never leaves the
  IDE, and the code alone is useless, so the relay stays blind in the
  sense of ADR 0087.
- **The redirect lands in whatever browser the phone opened**, which
  on iOS doesn't share storage with the installed app. Nothing needs
  to happen there: the page only says to go back.
- **Refreshes use the client the token was issued to**, stored with the
  token. Desktops keep moon-ide's registered app and the device flow.
- **A bridge Hugging Face can't reach** (an IP, `.local`) falls back to
  the device flow, link and code shown on the phone.
- CIMD clients get the Hub's fixed MCP scope set, which lacks
  `read-billing`: a phone sign-in has no billing-org picker.

## Rejected

- **Device flow only.** Works, but the code copying is the friction
  this removes.
- **The phone exchanging the code.** Puts the token on the phone and
  through the relay, and the callback page on iOS can't reach the
  app's pairing to forward it anyway.
- **A registered OAuth app per bridge.** Registration is manual and
  per host; CIMD needs none.
