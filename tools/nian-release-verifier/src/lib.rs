use base64::{Engine as _, engine::general_purpose::STANDARD};
use minisign_verify::{PublicKey, Signature};
use std::path::Path;

fn decode_wrapped_text(value: &str, label: &str) -> Result<String, String> {
    let decoded = STANDARD
        .decode(value.trim())
        .map_err(|_| format!("{label} is not valid base64"))?;
    String::from_utf8(decoded).map_err(|_| format!("{label} is not valid UTF-8"))
}

/// Verifies a Tauri v2 updater signature using the same Minisign-compatible
/// representation as `tauri-plugin-updater`.
pub fn verify_bytes(
    artifact: &[u8],
    signature_base64: &str,
    public_key_base64: &str,
) -> Result<(), String> {
    let public_key_text = decode_wrapped_text(public_key_base64, "updater public key")?;
    let public_key = PublicKey::decode(&public_key_text)
        .map_err(|_| "updater public key could not be decoded".to_string())?;

    let signature_text = decode_wrapped_text(signature_base64, "updater signature")?;
    let signature = Signature::decode(&signature_text)
        .map_err(|_| "updater signature could not be decoded".to_string())?;

    public_key
        .verify(artifact, &signature, true)
        .map_err(|_| "updater signature verification failed".to_string())
}

pub fn verify_file(
    artifact_path: &Path,
    signature_path: &Path,
    public_key_base64: &str,
) -> Result<(), String> {
    let artifact = std::fs::read(artifact_path).map_err(|_| {
        format!(
            "release artifact could not be read: {}",
            artifact_path.display()
        )
    })?;
    let signature = std::fs::read_to_string(signature_path).map_err(|_| {
        format!(
            "release signature could not be read: {}",
            signature_path.display()
        )
    })?;
    verify_bytes(&artifact, &signature, public_key_base64)
}

#[cfg(test)]
mod tests {
    use super::verify_bytes;

    const ARTIFACT: &[u8] = include_bytes!("../tests/fixtures/artifact.bin");
    const PUBLIC_A: &str = include_str!("../tests/fixtures/key-a.pub.b64");
    const PUBLIC_B: &str = include_str!("../tests/fixtures/key-b.pub.b64");
    const SIGNATURE_A: &str = include_str!("../tests/fixtures/artifact.bin.sig");

    #[test]
    fn signature_from_fixture_private_key_a_verifies_with_public_key_a() {
        assert!(verify_bytes(ARTIFACT, SIGNATURE_A, PUBLIC_A).is_ok());
    }

    #[test]
    fn signature_from_key_a_fails_with_public_key_b() {
        assert!(verify_bytes(ARTIFACT, SIGNATURE_A, PUBLIC_B).is_err());
    }

    #[test]
    fn mutated_artifact_fails_signature_verification() {
        let mut mutated = ARTIFACT.to_vec();
        mutated[0] ^= 0x01;
        assert!(verify_bytes(&mutated, SIGNATURE_A, PUBLIC_A).is_err());
    }

    #[test]
    fn mutated_signature_fails_verification() {
        let mut mutated = SIGNATURE_A.trim().as_bytes().to_vec();
        let index = mutated.len() / 2;
        mutated[index] = if mutated[index] == b'A' { b'B' } else { b'A' };
        let mutated = String::from_utf8(mutated).expect("base64 test fixture is ASCII");
        assert!(verify_bytes(ARTIFACT, &mutated, PUBLIC_A).is_err());
    }
}
