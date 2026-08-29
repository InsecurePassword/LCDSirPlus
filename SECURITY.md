# Security

## Scope

LCDSirPlus is a per-user, local-first Windows application. It opens the selected
G13 HID interface, optional local provider interfaces, Discord Desktop's local
named pipe, and explicitly configured outbound endpoints. It does not install a
service, driver, listener, browser extension, or elevated component.
Discord RPC authorization sends no redirect URI and opens no callback listener.

Telemetry is native-first. LCDSirPlus uses documented Win32/vendor APIs and
never probes raw MSRs, SMBus, EC, or Super-I/O registers. Optional HWiNFO access
is read-only shared memory; LCDSirPlus does not start or configure it. Optional
LibreHardwareMonitor access is HTTP restricted to loopback, but the user is
responsible for safely enabling and operating that web server.

Packages bundle the official Intel-signed PresentMon v2.5.1 console and exact
`licenses/PresentMon/LICENSE.txt`/`THIRD_PARTY.txt` notices. LCDSirPlus validates
the automatic artifact, starts it without a shell only for active frame capture,
and terminates only its owned child. It installs no PresentMon service, MSI,
GUI, or API. Local ETW policy can require Performance Log Users membership or
elevation for capture; neither is granted by LCDSirPlus.

LCDSirPlus packages are not code-signed even though the bundled PresentMon
binary is. Verify `LCDSirPlus-0.3.0-SHA256SUMS.txt` and the package's sorted
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
automatically when a continuous physical-button hold reaches its configured
threshold. Releasing afterward only resets the hold. Use safe mode while
investigating. Test only with the disposable harness documented in
`docs/HARDWARE-ACCEPTANCE.md`; never target unsaved work.
