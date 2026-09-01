# Security

## Scope

LCDSirPlus is a local-first Windows application with current-user and optional
all-users installation modes. It opens the selected
G13 HID interface, optional local provider interfaces, Discord Desktop's local
named pipe, and explicitly configured outbound endpoints. It does not install a
service, driver, listener, or browser extension. All-users setup uses normal UAC
elevation; the installed application and its per-user startup task run limited.
Discord RPC authorization sends no redirect URI and opens no callback listener.

Telemetry is native-first. LCDSirPlus uses documented Win32/vendor APIs and
never probes raw MSRs, SMBus, EC, or Super-I/O registers. Optional HWiNFO access
is read-only shared memory; LCDSirPlus does not start or configure it. Optional
LibreHardwareMonitor access is HTTP restricted to loopback, but the user is
responsible for safely enabling and operating that web server. LHM is
user-managed and not bundled; its absence is an acceptable unavailable state.

Planned release packages bundle the official Intel-signed PresentMon v2.5.1
console and exact `licenses/PresentMon/LICENSE.txt`/`THIRD_PARTY.txt` notices. LCDSirPlus validates
the automatic artifact, starts it without a shell only for active frame capture,
and terminates only its owned child. It installs no PresentMon service, MSI,
GUI, or API. Local ETW policy can require Performance Log Users membership or
elevation for capture; neither is granted by LCDSirPlus.

Published LCDSirPlus packages will not be code-signed even though the bundled PresentMon
binary is. Verify `LCDSirPlus-0.3.0-SHA256SUMS.txt` and the package's sorted
`PACKAGE-MANIFEST.txt` before running ZIP contents, and verify the setup EXE's
outer checksum before running it. Setup refuses the same SID's opposite-scope
registration, legacy PowerShell installs, and unverified scheduled-task
collisions; it does not claim to enumerate offline user hives. Another SID's
all-users registration may coexist with a current-user install. It never
overwrites or removes a task until the stored SID/name and exact product source,
stable description, action, working directory, trigger, principal, logon type,
and run level are authenticated. Create/update preserves the exact prior task
definition and restores it if registration or post-verification fails. Uninstall
validates without mutation at startup, then revalidates and removes the task only
when file removal commits, so cancellation leaves the task intact.

Diagnostics are local, offline, capped, and allowlist-only. They ignore
`--config` and exclude raw configuration/logs, credentials, identifiers,
titles, paths, addresses, environment values, command lines, registry values,
serials, arbitrary listings, and device paths. Review a bundle before sharing.

## Secrets

Each user must create and register their own Discord application. Never put
Discord client secrets or tokens in source, configuration, command arguments,
issues, chat, diagnostics, logs, screenshots, or package fixtures. The generic
OAuth exchange used by LCDSirPlus requires the client secret; Public Client
no-secret authorization is Social SDK-specific and is not implemented. Enter
the secret through a masked prompt, expose it only through
`LCDSIRPLUS_DISCORD_CLIENT_SECRET` for authorization, and remove the environment
variable immediately afterward. The secret is retained only inside that
account's DPAPI-protected v2 credential so refresh can use it. Credentials stay
in current-user DPAPI-protected local files
under `%LOCALAPPDATA%\LCDSirPlus`; installed configuration is separately stored
at `%LOCALAPPDATA%\LCDSirPlus\Config\lcdsirplus.txt`, while portable/development
configuration is adjacent to the executable. Explicit `--config` takes
precedence; the installed template seeds only a missing user file on first use
and does not overwrite it. Runtime accepts a credential only when its immutable Discord
user ID and client ID match the trusted pipe's current `READY` session and the
authenticated user has all required scopes. Revoke exposed credentials with
Discord.

Discord support is implemented and software-tested, but live voice/OAuth
qualification remains pending unless explicitly deferred; this document does
not claim that live gate has passed.

## Reporting

If GitHub private vulnerability reporting is enabled and available, use
<https://github.com/InsecurePassword/LCDSirPlus/security/advisories/new>. This
URL does not imply that the repository feature is enabled. Include the affected
version, reproduction steps, impact, and whether credentials or destructive
actions are involved, but never include live secrets or personal diagnostics.

If private reporting is unavailable, open
<https://github.com/InsecurePassword/LCDSirPlus/issues/new> with no sensitive
details and request a private contact channel. Allow time for triage and a
corrected release before public disclosure.

## Local Safety

The hung-window action is destructive and disabled by default. `PROC_HANG`
remains selected in its shipped slot; its provider reports disabled/unavailable
and its no-target pane falls through until the user explicitly sets
`hang_enabled 1`. When enabled, it can terminate an exact,
repeatedly revalidated process automatically when a continuous physical-button
hold reaches its configured threshold. Releasing afterward only resets the
hold. The action remains unqualified until the disposable-child physical-button
gate passes. Use safe mode while investigating, test only with the harness in
`docs/HARDWARE-ACCEPTANCE.md`, and never target unsaved work.
