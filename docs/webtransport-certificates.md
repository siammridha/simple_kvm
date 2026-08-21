# WebTransport and certificates: what actually works

Finding from testing on 2026-08-21, against the real device (v0.1.15) with a
private root CA (step-ca style) manually trusted in the browser.

## The problem

Trusting a private root CA in a browser makes normal HTTPS work. It does
**not** make WebTransport work, unless the WebTransport connection uses
`serverCertificateHashes`.

This is a hard rule enforced by the browser, not a bug or a misconfiguration.

## Why

WebTransport connections can be verified two ways:

1. **`serverCertificateHashes`** - the page pins the exact SHA-256 hash of
   the server's certificate. The browser skips normal CA checking entirely.
   In exchange, the certificate must be valid for 14 days or less.
2. **Normal certificate checking** (no `serverCertificateHashes`) - the
   browser checks the certificate the same way it would for HTTPS, plus one
   extra requirement: the certificate must be logged in **Certificate
   Transparency (CT)**, a set of public logs that only publicly trusted CAs
   (Let's Encrypt, DigiCert, etc.) submit to.

A private CA you run yourself can never be in a CT log - CT logs don't
accept certificates from private CAs. So a private-CA certificate fails
WebTransport's normal-checking path even when the root is fully trusted on
the device. This applies specifically to WebTransport; regular HTTPS in the
same browser does not enforce CT for a manually trusted private root.

## How this was confirmed

Tested against `https://simplekvm.com/` (device at 192.168.10.20, v0.1.15,
no `serverCertificateHashes` in this version's code):

| Check | Result |
|---|---|
| Root CA imported and trusted in Firefox and Chromium | done |
| Plain HTTPS fetch on port 443 (same certificate) | succeeds |
| Raw QUIC/TLS handshake on port 4433 (`openssl s_client -quic`) | succeeds, full chain verifies, ALPN `h3` negotiates |
| `new WebTransport(...)` in headless Firefox | fails: "WebTransport connection rejected" |
| `new WebTransport(...)` in Chromium | fails: "Opening handshake failed" |
| Device's server log during the failed attempts | no entry - the browser rejects before the app ever sees the request |

Both browsers reject it, before the connection even reaches the app. The
certificate and chain are provably fine at the TLS level. That combination
points at the browser-side WebTransport-specific check, not a setup mistake.

## The two real options

1. **Get a certificate from a public CA** (e.g. Let's Encrypt) for a domain
   actually owned, with a real public DNS record - even if the server itself
   only listens on the LAN. This satisfies CT and lets WebTransport use
   normal checking.
2. **Use `serverCertificateHashes`** with a certificate valid for 14 days or
   less, rotated automatically before it expires.

Sources:
- https://www.idmanagement.gov/implement/announcements/03_google_ct/
- https://chromestatus.com/feature/5690646332440576
- https://groups.google.com/a/chromium.org/g/blink-dev/c/m0v9XiwKA4M/m/GtMq9j_iAAAJ
