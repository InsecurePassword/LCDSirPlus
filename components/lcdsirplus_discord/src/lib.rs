//! `lcdsirplus_discord` — the optional Discord component of LCDSirPlus (WO-TEL-D3-C).
//!
//! Built as a single `cdylib`. LCDSirPlus loads it at run time only when Discord is enabled AND a
//! client id is configured (the ratified consumer rule: the speaker overlay reads the state every
//! frame) — or for the one-shot `--discord-authorize` command. At disabled/unconfigured the module
//! is never loaded, so no Discord thread, RPC, credential touch or allocation exists.
//!
//! ABI v1: one export (`lcsd_get_table`) + one size-checked, ten-function table of
//! `extern "system"` pointers, POD payloads with capacities, opaque DLL-owned handle, status-code
//! returns. No Rust type, `String`, `Vec`, reference, allocator handle, `dyn` or panic crosses the
//! boundary; the profile is `panic = "abort"`, so every entry point is total by construction.
//! The worker thread, the credential store (DPAPI + `CredentialBoundary`) and the Discord IPC
//! server-identity/Authenticode verifier live in the executable, reached through the v1
//! `LcsdHostApi`, so no component code runs in a thread the host cannot join.

pub mod abi;
#[cfg(test)]
mod boundary;
mod engine;
mod json;
mod rpc;
mod types;
