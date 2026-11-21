// SHARED-RULES-START (WO-TEL-D3-C: boundary-shared identity rules; this block is byte-identical
// in src/providers/discord.rs (host) and here (component); pinned by
// discord_shared_rules_stay_byte_identical_across_the_boundary in both crates, so the credential
// path rule and the cryptographic nonce rule cannot drift between the two packages.)
fn validate_user_id(id: &str) -> Result<&str, String> {
    if id.is_empty()
        || id.len() > 20
        || !id.bytes().all(|byte| byte.is_ascii_digit())
        || id.starts_with('0')
        || id.parse::<u64>().is_err()
    {
        return Err("Discord user ID is not a canonical numeric Snowflake".into());
    }
    Ok(id)
}
fn random_nonce() -> Result<String, String> {
    use windows::Win32::Security::Cryptography::{
        BCryptGenRandom, BCRYPT_ALG_HANDLE, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    };
    let mut bytes = [0u8; 16];
    let status = unsafe {
        BCryptGenRandom(
            BCRYPT_ALG_HANDLE::default(),
            &mut bytes,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status.0 < 0 {
        return Err("Windows cryptographic RNG failed".into());
    }
    Ok(bytes.iter().map(|value| format!("{:02x}", value)).collect())
}// SHARED-RULES-END
