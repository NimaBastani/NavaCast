use crate::error::Error;
use argon2::{Algorithm, Argon2, Params, Version};
use base64::engine::general_purpose::STANDARD_NO_PAD;
use base64::Engine;
use sha2::{Digest, Sha256};

#[derive(Clone, Copy)]
pub struct HashParams {
    pub memory_kib: u32,
    pub time: u32,
    pub threads: u32,
}

impl HashParams {
    pub fn strong() -> Self {
        Self { memory_kib: 19 * 1024, time: 2, threads: 1 }
    }
    pub fn fast() -> Self {
        Self { memory_kib: 8 * 1024, time: 1, threads: 1 }
    }
}

pub fn hash_password(password: &str) -> Result<String, Error> {
    hash_with(password, HashParams::strong())
}

pub fn hash_with(password: &str, p: HashParams) -> Result<String, Error> {
    if password.is_empty() || password.len() > 256 {
        return Err(Error::Invalid("password length".into()));
    }
    let mut salt = [0u8; 16];
    getrandom::getrandom(&mut salt).map_err(|e| Error::Other(e.to_string()))?;
    let params = Params::new(p.memory_kib, p.time, p.threads, Some(32))
        .map_err(|e| Error::Other(e.to_string()))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut out = [0u8; 32];
    hash_into(&argon, password.as_bytes(), &salt, &mut out).map_err(|e| Error::Other(e.to_string()))?;
    Ok(format!(
        "$argon2id$v=19$m={},t={},p={}${}${}",
        p.memory_kib,
        p.time,
        p.threads,
        STANDARD_NO_PAD.encode(salt),
        STANDARD_NO_PAD.encode(out)
    ))
}

/// Verify an argon2id PHC string. Malformed or hostile parameter sets return false.
pub fn verify_password(encoded: &str, password: &str) -> bool {
    if password.is_empty() || password.len() > 256 {
        return false;
    }
    let Some((mem, time, threads, salt, want)) = parse_phc(encoded) else {
        return false;
    };
    if mem > 256 * 1024 || time > 8 || threads == 0 || threads > 4 || salt.len() < 8 || want.len() < 16 {
        return false;
    }
    let Ok(params) = Params::new(mem, time, threads, Some(want.len())) else {
        return false;
    };
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut got = vec![0u8; want.len()];
    if hash_into(&argon, password.as_bytes(), &salt, &mut got).is_err() {
        return false;
    }
    constant_time_eq(&got, &want)
}

fn hash_into(argon: &Argon2, password: &[u8], salt: &[u8], out: &mut [u8]) -> Result<(), argon2::Error> {
    let mut memory = vec![argon2::Block::default(); argon.params().block_count()];
    argon.hash_password_into_with_memory(password, salt, out, &mut memory)
}

fn parse_phc(encoded: &str) -> Option<(u32, u32, u32, Vec<u8>, Vec<u8>)> {
    let parts: Vec<&str> = encoded.split('$').collect();
    if parts.len() != 6 || parts[1] != "argon2id" || parts[2] != "v=19" {
        return None;
    }
    let mut mem = 0u32;
    let mut time = 0u32;
    let mut threads = 0u32;
    for kv in parts[3].split(',') {
        let (k, v) = kv.split_once('=')?;
        let n: u32 = v.parse().ok()?;
        match k {
            "m" => mem = n,
            "t" => time = n,
            "p" => threads = n,
            _ => return None,
        }
    }
    if mem == 0 || time == 0 || threads == 0 {
        return None;
    }
    let salt = STANDARD_NO_PAD.decode(parts[4]).ok()?;
    let sum = STANDARD_NO_PAD.decode(parts[5]).ok()?;
    Some((mem, time, threads, salt, sum))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub fn new_token() -> Result<String, Error> {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).map_err(|e| Error::Other(e.to_string()))?;
    Ok(format!("nv_{}", STANDARD_NO_PAD.encode(buf)))
}

pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let dig = hasher.finalize();
    dig.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn random_password() -> Result<String, Error> {
    let mut buf = [0u8; 18];
    getrandom::getrandom(&mut buf).map_err(|e| Error::Other(e.to_string()))?;
    Ok(STANDARD_NO_PAD.encode(buf))
}

pub fn new_id() -> String {
    let mut buf = [0u8; 16];
    getrandom::getrandom(&mut buf).expect("rng");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argon2id_roundtrip() {
        let h = hash_with("correct horse", HashParams::fast()).unwrap();
        assert!(h.starts_with("$argon2id$"));
        assert!(verify_password(&h, "correct horse"));
        assert!(!verify_password(&h, "wrong"));
        assert!(!verify_password("nope", "correct horse"));
        assert!(!verify_password(&h, ""));
        assert!(!verify_password("$argon2id$v=19$m=99999999,t=100,p=1$aaaa$bbbb", "x"));
    }

    #[test]
    fn token_shape() {
        let t = new_token().unwrap();
        assert!(t.starts_with("nv_"));
        assert_ne!(hash_token(&t), hash_token(&(t.clone() + "x")));
    }
}
