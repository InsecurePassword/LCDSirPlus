# Security

## Scope

LCDSirPlus is a per-user, local-first Windows application. It opens the selected
G13 HID interface, optional local provider interfaces, Discord Desktop's local
named pipe, and explicitly configured outbound endpoints. It does not install a
service, driver, listener, browser extension, or elevated component.

Packages are not code-signed. Verify `LCDSirPlus-0.3.0-SHA256SUMS.txt` and the package's sorted
`PACKAGE-MANIFEST.txt` before running an executable or installer. The installer
performs the same member hash, size, name, reparse, and hard-link checks and
refuses foreign Start Menu or HKCU Run ownership.

Diagnostics are local, offline, capped, and allowlist-only. They ignore
`--config` and exclude raw configuration/logs, credentials, identifiers,
titles, paths, addresses, environment values, command lines, registry values,
serials, arbitrary listings, and device paths. Review a bundle before sharing.

## Secrets

Never put Discord client secrets or tokens in configuration, command arguments,
issues, diagnostics, logs, screenshots, or package fixtures. If Discord requires
a client secret, expose it only through `LCDSIRPLUS_DISCORD_CLIENT_SECRET` for
authorization and remove the environment variable immediately afterward. The
secret is retained only inside that account's DPAPI-protected v2 credential so
refresh can use it. Runtime accepts a credential only when its immutable Discord
user ID and client ID match the trusted pipe's current `READY` session and the
authenticated user has all required scopes. Revoke exposed credentials with
Discord.

## Reporting

Report vulnerabilities privately to the project maintainer with the affected
version, reproduction steps, impact, and whether credentials or destructive
actions are involved. Do not include live secrets or personal diagnostics.
Allow time for triage and a corrected release before public disclosure.

## Local Safety

The hung-window action can terminate an exact, repeatedly revalidated process
only after a continuous physical-button hold and release. Use safe mode while
investigating. Test only with the disposable harness documented in
`docs/HARDWARE-ACCEPTANCE.md`; never target unsaved work.
