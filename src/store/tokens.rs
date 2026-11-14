use base64::Engine as _;
use sha2::{Digest, Sha256};

use super::Store;

pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn preview_token(token: &str) -> Option<String> {
    if token.is_empty() {
        return None;
    }
    let chars: Vec<char> = token.chars().collect();
    if chars.len() <= 12 {
        let head: String = chars.iter().take(2).collect();
        let tail: String = chars.iter().skip(chars.len() - 2).collect();
        return Some(format!("{head}••••{tail}"));
    }
    let head: String = chars.iter().take(5).collect();
    let tail: String = chars.iter().skip(chars.len() - 4).collect();
    Some(format!("{head}••••{tail}"))
}

pub fn create_worker_token() -> String {
    use rand::Rng;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    let a = left.as_bytes();
    let b = right.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

impl Store {
    /// Resolves the worker access key. Returns plaintext only when a new key
    /// was generated, so the caller can print it exactly once.
    pub fn ensure_worker_token(&mut self, env_token: &str) -> Result<Option<String>, String> {
        if !env_token.is_empty() {
            let hash = hash_token(env_token);
            if self.state.worker_token_hash.as_deref() != Some(hash.as_str()) {
                self.state.worker_token_hash = Some(hash);
                self.state.worker_token_preview = preview_token(env_token);
                self.persist_token()?;
            }
            return Ok(None);
        }

        if self.state.worker_token_hash.is_some() {
            return Ok(None);
        }

        let token = create_worker_token();
        self.state.worker_token_hash = Some(hash_token(&token));
        self.state.worker_token_preview = preview_token(&token);
        self.persist_token()?;
        Ok(Some(token))
    }

    pub fn verify_worker_token(&self, token: &str) -> bool {
        if token.is_empty() {
            return false;
        }
        match self.state.worker_token_hash.as_deref() {
            Some(hash) => constant_time_eq(&hash_token(token), hash),
            None => false,
        }
    }

    pub fn rotate_worker_token(&mut self) -> Result<String, String> {
        let token = create_worker_token();
        self.state.worker_token_hash = Some(hash_token(&token));
        self.state.worker_token_preview = preview_token(&token);
        self.persist_token()?;
        Ok(token)
    }

    fn persist_token(&self) -> Result<(), String> {
        if let Some(hash) = &self.state.worker_token_hash {
            self.set_meta("worker_token_hash", hash)?;
        }
        if let Some(preview) = &self.state.worker_token_preview {
            self.set_meta("worker_token_preview", preview)?;
        }
        Ok(())
    }
}
