//! Embeds the payload (SeaBIOS, SeaVGABIOS, the TempleOS ISO) and refuses to
//! build unless each file matches its pinned SHA-256 (payload/SHA256SUMS and
//! payload/TempleOS.ISO.pin). The pinned hashes are also handed to the
//! program (PAYLOAD_SHA256_<NAME>), which checks the embedded copies again
//! every time it starts.

use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

fn sha256(path: &Path) -> String {
    let data = fs::read(path).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e}\nFetch the payload first: tools/fetch-payload.sh",
            path.display()
        )
    });
    Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect()
}

fn check(path: &Path, want: &str) {
    println!("cargo:rerun-if-changed={}", path.display());
    let got = sha256(path);
    if got != want {
        panic!("{}: sha256 {got} does not match the pinned {want}", path.display());
    }
}

fn main() {
    let payload = Path::new("payload");
    println!("cargo:rerun-if-changed=payload/SHA256SUMS");
    println!("cargo:rerun-if-changed=payload/TempleOS.ISO.pin");

    let sums = fs::read_to_string(payload.join("SHA256SUMS")).expect("payload/SHA256SUMS");
    for line in sums.lines().filter(|l| !l.trim().is_empty()) {
        let (hash, name) = line.split_once(char::is_whitespace).expect("SHA256SUMS line");
        let name = name.trim().trim_start_matches('*');
        check(&payload.join(name), hash);
        emit(name, hash);
    }

    let pin = fs::read_to_string(payload.join("TempleOS.ISO.pin")).expect("payload/TempleOS.ISO.pin");
    let iso_sha = pin
        .lines()
        .find_map(|l| l.strip_prefix("sha256="))
        .filter(|s| !s.is_empty())
        .expect("payload/TempleOS.ISO.pin has no sha256; run tools/fetch-payload.sh");
    check(&payload.join("TempleOS.ISO"), iso_sha.trim());
    emit("TempleOS.ISO", iso_sha.trim());
}

/// PAYLOAD_SHA256_BIOS for bios.bin, and so on.
fn emit(name: &str, hash: &str) {
    let key: String = name.split('.').next().unwrap().to_uppercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    println!("cargo:rustc-env=PAYLOAD_SHA256_{key}={hash}");
}
