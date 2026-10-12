use sha1::Digest as _;

/// `Hash.fast`: SHA-1 hex digest.
pub fn fast(input: impl AsRef<[u8]>) -> String {
    hex::encode(sha1::Sha1::digest(input.as_ref()))
}

/// `Hash.sha256`: SHA-256 hex digest.
pub fn sha256(input: impl AsRef<[u8]>) -> String {
    hex::encode(sha2::Sha256::digest(input.as_ref()))
}
