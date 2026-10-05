//! Records the build target for the updater (it downloads the release asset
//! for the same target) and embeds the release signing key from
//! `minisign.pub` when present. Builds without the key cannot install updates.

fn main() {
    println!("cargo:rustc-env=ROMMD_TARGET={}", std::env::var("TARGET").unwrap());
    println!("cargo:rerun-if-changed=minisign.pub");
    if let Ok(key) = std::fs::read_to_string("minisign.pub") {
        // The key file is two lines: an untrusted comment, then the key.
        if let Some(line) = key.lines().map(str::trim).find(|l| l.starts_with("RW")) {
            println!("cargo:rustc-env=ROMMD_UPDATE_PUBKEY={}", line);
        }
    }
}
