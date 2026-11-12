//! `bongos-kotlin-manifest`: creates the Ed25519 signing key for the remote Kotlin server manifest,
//! signs `servers.json` files and checks them the way the app does.
//!
//!   keygen [--id <id>] <private key path>   write a new key (mode 0600, never overwrites a file)
//!                                           and print its id and public key
//!   sign <private key path> <key id> <servers.json>...
//!                                           write or extend `<file>.sig` with a signature by that key
//!   verify <servers.json> <id:public key>   check `<file>.sig` through the app's own verifier
//!
//! The private key is never printed.

use std::{
    ffi::OsString,
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use bongos_kotlin::{RemoteManifest, TrustedKey};
use http_client::Url;
use ring::{
    rand::SystemRandom,
    signature::{Ed25519KeyPair, KeyPair as _},
};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

const USAGE: &str = "usage:
  bongos-kotlin-manifest keygen [--id <id>] <private key path>
  bongos-kotlin-manifest sign <private key path> <key id> <servers.json>...
  bongos-kotlin-manifest verify <servers.json> <id:public key>";

/// Where `verify` pretends the manifest was downloaded from. Archive URLs may name its host, so a
/// manifest that passes here also passes with archives on `download.jetbrains.com`.
const VERIFY_MANIFEST_URL: &str = "https://download.jetbrains.com/bongos/kotlin/servers.json";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(output) => {
            let mut stdout = io::stdout();
            let _ = stdout.write_all(output.as_bytes());
            let _ = stdout.flush();
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("bongos-kotlin-manifest: {error:#}");
            ExitCode::FAILURE
        }
    }
}

/// Runs one command line (without the program name) and returns what it prints.
fn run(args: &[String]) -> Result<String> {
    let usage = || anyhow!("{USAGE}");
    let (command, rest) = args.split_first().ok_or_else(usage)?;
    match (command.as_str(), rest) {
        ("keygen", [path]) => keygen_report(Path::new(path), None),
        ("keygen", [flag, id, path]) if flag == "--id" => keygen_report(Path::new(path), Some(id)),
        ("sign", [key, id, files @ ..]) if !files.is_empty() => {
            let mut output = String::new();
            for file in files {
                let sig = sign_file(Path::new(key), id, Path::new(file))?;
                output.push_str(&format!("signed {file} with key {id}: {}\n", sig.display()));
            }
            Ok(output)
        }
        ("verify", [file, key]) => {
            let manifest = verify_file(Path::new(file), key)?;
            Ok(format!(
                "ok\nserial {}, {} kotlin-lsp build(s)\n",
                manifest.serial,
                manifest.kotlin_lsp.len()
            ))
        }
        _ => Err(usage()),
    }
}

/// A key id: nonempty printable ASCII without ':' (the `<id>:<public key>` separator).
fn check_key_id(id: &str) -> Result<()> {
    ensure!(!id.is_empty(), "the key id is empty");
    ensure!(
        id.bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b':'),
        "the key id {id:?} must be printable ASCII without ':' or spaces"
    );
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The default key id: the first 8 hex characters of the SHA-256 of the public key.
fn default_key_id(public_key: &[u8]) -> String {
    hex(&Sha256::digest(public_key))[..8].to_string()
}

struct GeneratedKey {
    id: String,
    public_key: String,
}

/// Creates the private key file (PKCS#8) with mode 0600 from the first moment it exists, and
/// refuses to touch a file that is already there.
fn keygen(path: &Path, id: Option<&str>) -> Result<GeneratedKey> {
    if let Some(id) = id {
        check_key_id(id)?;
    }
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
        .map_err(|_| anyhow!("the system random number generator failed"))?;
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
        .map_err(|_| anyhow!("the generated key is not a valid Ed25519 key"))?;

    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path).with_context(|| {
        format!(
            "creating {} (an existing file is never overwritten)",
            path.display()
        )
    })?;
    if let Err(error) = file
        .write_all(pkcs8.as_ref())
        .and_then(|()| file.sync_all())
    {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error).with_context(|| format!("writing {}", path.display()));
    }

    let public_key = pair.public_key().as_ref();
    Ok(GeneratedKey {
        id: id.map_or_else(|| default_key_id(public_key), str::to_string),
        public_key: BASE64.encode(public_key),
    })
}

/// The text `keygen` prints: the key id and the public key, nothing about the private key.
fn keygen_report(path: &Path, id: Option<&str>) -> Result<String> {
    let key = keygen(path, id)?;
    Ok(format!(
        "key id: {}\npublic key: {}\ntrusted key: {}:{}\n",
        key.id, key.public_key, key.id, key.public_key
    ))
}

fn load_key(path: &Path) -> Result<Ed25519KeyPair> {
    let bytes = fs::read(path).with_context(|| format!("reading the key {}", path.display()))?;
    Ed25519KeyPair::from_pkcs8(&bytes)
        .map_err(|_| anyhow!("{} is not an Ed25519 PKCS#8 private key", path.display()))
}

/// `<file>.sig`.
fn signature_path(file: &Path) -> PathBuf {
    let mut name: OsString = file.as_os_str().to_owned();
    name.push(".sig");
    PathBuf::from(name)
}

/// Signs the exact bytes of `file` and writes the signature into `<file>.sig`, keeping the other
/// entries of an existing signature file and replacing any entry with the same key id. Returns the
/// signature file's path.
fn sign_file(key_path: &Path, key_id: &str, file: &Path) -> Result<PathBuf> {
    check_key_id(key_id)?;
    let pair = load_key(key_path)?;
    let bytes = fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let sig_path = signature_path(file);

    let mut signatures = match fs::read(&sig_path) {
        Ok(existing) => {
            let parsed: Value = serde_json::from_slice(&existing)
                .with_context(|| format!("parsing the existing {}", sig_path.display()))?;
            match parsed.get("signatures") {
                Some(Value::Array(entries)) => entries.clone(),
                _ => bail!(
                    "the existing {} has no \"signatures\" list",
                    sig_path.display()
                ),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", sig_path.display()));
        }
    };
    signatures.retain(|entry| entry.get("key").and_then(Value::as_str) != Some(key_id));
    signatures.push(json!({
        "key": key_id,
        "ed25519": BASE64.encode(pair.sign(&bytes)),
    }));

    let mut text = serde_json::to_string_pretty(&json!({ "signatures": signatures }))?;
    text.push('\n');
    fs::write(&sig_path, text).with_context(|| format!("writing {}", sig_path.display()))?;
    Ok(sig_path)
}

/// Checks `<file>.sig` with the app's verifier (`RemoteManifest::verify_and_parse`) against the key
/// `trusted` (`<id>:<base64 public key>`), and fails if no build in the manifest passes the app's
/// checks, since the app would skip them all.
fn verify_file(file: &Path, trusted: &str) -> Result<RemoteManifest> {
    let (id, public_key) = trusted
        .split_once(':')
        .ok_or_else(|| anyhow!("the key must be <id>:<base64 public key>"))?;
    let key = TrustedKey::parse(id, public_key)?;
    let manifest = fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let sig_path = signature_path(file);
    let signature = match fs::read(&sig_path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", sig_path.display()));
        }
    };
    let url = Url::parse(VERIFY_MANIFEST_URL)?;
    let parsed = RemoteManifest::verify_and_parse(&manifest, signature.as_deref(), &url, &[key])?;
    ensure!(
        !parsed.kotlin_lsp.is_empty(),
        "the signature is valid, but no kotlin-lsp entry passes the app's checks"
    );
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    const DIGEST: &str = "ae0c4b01e17ab73a4a5227a78ea99e6dcbe3e499600315b22759a4e4cddd11df";

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| part.to_string()).collect()
    }

    fn servers_json() -> String {
        let url = "https://download.jetbrains.com/language-server/kotlin-server/263.8000.0/kotlin-server-263.8000.0.tar.gz";
        serde_json::to_string_pretty(&json!({
            "schema": 1, "kind": "bongos-kotlin-servers", "serial": 7, "published": "2026-11-11",
            "kotlin-lsp": [{
                "version": "263.8000.0", "built": "2026-11-10", "expires": "2026-12-09",
                "requires": 1,
                "platform": { "linux-x64": {
                    "url": url, "sha256": DIGEST, "archive": "tar.gz", "strip_components": 1,
                    "launcher": "bin/intellij-server", "runtime": "jbr/bin/java"
                }}
            }]
        }))
        .unwrap()
    }

    /// Runs `keygen` and returns (key id, public key, the trusted `<id>:<key>`).
    fn make_key(dir: &Path, name: &str) -> (PathBuf, String, String) {
        let path = dir.join(name);
        let output = run(&args(&["keygen", path.to_str().unwrap()])).unwrap();
        let field = |label: &str| {
            output
                .lines()
                .find_map(|line| line.strip_prefix(label))
                .unwrap()
                .to_string()
        };
        (path, field("key id: "), field("trusted key: "))
    }

    #[test]
    fn test_keygen_is_private_and_prints_only_public_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("release.key");
        let output = run(&args(&["keygen", path.to_str().unwrap()])).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let private = fs::read(&path).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(&private).unwrap();
        let public_key = BASE64.encode(pair.public_key().as_ref());
        let id = default_key_id(pair.public_key().as_ref());
        assert!(output.contains(&format!("key id: {id}\n")), "{output}");
        assert!(output.contains(&public_key), "{output}");

        // Nothing derived from the private bytes (ring's PKCS#8 holds the 32-byte seed at 16..48).
        let seed = &private[16..48];
        for secret in [&private[..], seed] {
            assert!(!output.contains(&BASE64.encode(secret)));
            assert!(!output.contains(&hex(secret)));
        }
    }

    #[test]
    fn test_keygen_refuses_to_overwrite_and_takes_an_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("release.key");
        fs::write(&path, b"existing").unwrap();
        assert!(run(&args(&["keygen", path.to_str().unwrap()])).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"existing");

        let other = dir.path().join("other.key");
        let output = run(&args(&[
            "keygen",
            "--id",
            "release-1",
            other.to_str().unwrap(),
        ]))
        .unwrap();
        assert!(output.contains("key id: release-1\n"), "{output}");
        assert!(
            run(&args(&[
                "keygen",
                "--id",
                "a:b",
                dir.path().join("x").to_str().unwrap()
            ]))
            .is_err()
        );
    }

    #[test]
    fn test_sign_then_verify_through_the_apps_verifier() {
        let dir = tempfile::tempdir().unwrap();
        let (key, id, trusted) = make_key(dir.path(), "a.key");
        let file = dir.path().join("servers.json");
        fs::write(&file, servers_json()).unwrap();
        let output = run(&args(&[
            "sign",
            key.to_str().unwrap(),
            &id,
            file.to_str().unwrap(),
        ]))
        .unwrap();
        assert!(output.contains("servers.json.sig"), "{output}");

        let sig: Value =
            serde_json::from_slice(&fs::read(dir.path().join("servers.json.sig")).unwrap())
                .unwrap();
        assert_eq!(sig["signatures"][0]["key"], json!(id));
        let raw = BASE64
            .decode(sig["signatures"][0]["ed25519"].as_str().unwrap())
            .unwrap();
        assert_eq!(raw.len(), 64);

        let output = run(&args(&["verify", file.to_str().unwrap(), &trusted])).unwrap();
        assert!(output.starts_with("ok\n"), "{output}");

        // The app's verifier, called directly, agrees.
        let (_, public_key) = trusted.split_once(':').unwrap();
        let parsed = RemoteManifest::verify_and_parse(
            servers_json().as_bytes(),
            Some(&fs::read(dir.path().join("servers.json.sig")).unwrap()),
            &Url::parse("https://manifests.example.com/servers.json").unwrap(),
            &[TrustedKey::parse(&id, public_key).unwrap()],
        )
        .unwrap();
        assert_eq!(parsed.serial, 7);
        assert_eq!(parsed.newest("linux-x64").unwrap().version, "263.8000.0");
    }

    #[test]
    fn test_signing_twice_with_two_keys_keeps_both_and_resigning_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let (key_a, id_a, trusted_a) = make_key(dir.path(), "a.key");
        let (key_b, id_b, trusted_b) = make_key(dir.path(), "b.key");
        assert_ne!(id_a, id_b);
        let file = dir.path().join("servers.json");
        fs::write(&file, servers_json()).unwrap();
        let sig_path = dir.path().join("servers.json.sig");
        let sign = |key: &Path, id: &str| sign_file(key, id, &file).unwrap();
        sign(&key_a, &id_a);
        sign(&key_b, &id_b);
        let entries = |path: &Path| -> Vec<Value> {
            let value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
            value["signatures"].as_array().unwrap().clone()
        };
        assert_eq!(entries(&sig_path).len(), 2);
        // Either key alone verifies.
        verify_file(&file, &trusted_a).unwrap();
        verify_file(&file, &trusted_b).unwrap();

        // Signing again with the first key replaces its entry instead of adding one.
        sign(&key_a, &id_a);
        let after = entries(&sig_path);
        assert_eq!(after.len(), 2);
        assert!(after.iter().any(|entry| entry["key"] == json!(id_b)));
        verify_file(&file, &trusted_a).unwrap();
    }

    #[test]
    fn test_signing_keeps_entries_it_does_not_understand() {
        let dir = tempfile::tempdir().unwrap();
        let (key, id, trusted) = make_key(dir.path(), "a.key");
        let file = dir.path().join("servers.json");
        fs::write(&file, servers_json()).unwrap();
        let sig_path = dir.path().join("servers.json.sig");
        fs::write(
            &sig_path,
            br#"{"signatures": [{"key": "old", "ed25519": "AAAA", "x": 1}]}"#,
        )
        .unwrap();
        sign_file(&key, &id, &file).unwrap();
        let value: Value = serde_json::from_slice(&fs::read(&sig_path).unwrap()).unwrap();
        assert_eq!(value["signatures"][0]["x"], json!(1));
        verify_file(&file, &trusted).unwrap();

        // A signature file that is not ours is an error, not overwritten.
        fs::write(&sig_path, b"not json").unwrap();
        assert!(sign_file(&key, &id, &file).is_err());
        assert_eq!(fs::read(&sig_path).unwrap(), b"not json");
    }

    #[test]
    fn test_verify_fails_after_a_change_and_without_a_signature() {
        let dir = tempfile::tempdir().unwrap();
        let (key, id, trusted) = make_key(dir.path(), "a.key");
        let file = dir.path().join("servers.json");
        fs::write(&file, servers_json()).unwrap();
        assert!(verify_file(&file, &trusted).is_err(), "no .sig yet");

        sign_file(&key, &id, &file).unwrap();
        verify_file(&file, &trusted).unwrap();

        let mut changed = servers_json().into_bytes();
        let index = changed.iter().position(|byte| *byte == b'8').unwrap();
        changed[index] = b'9';
        fs::write(&file, &changed).unwrap();
        let error = verify_file(&file, &trusted).unwrap_err();
        assert!(format!("{error:#}").contains("no signature"), "{error:#}");

        // The right bytes but another key.
        fs::write(&file, servers_json()).unwrap();
        let (_, _, other) = make_key(dir.path(), "b.key");
        assert!(verify_file(&file, &other).is_err());
        assert!(verify_file(&file, "no-colon").is_err());
    }

    #[test]
    fn test_verify_fails_when_the_app_would_skip_every_entry() {
        let dir = tempfile::tempdir().unwrap();
        let (key, id, trusted) = make_key(dir.path(), "a.key");
        let file = dir.path().join("servers.json");
        fs::write(&file, servers_json().replace(DIGEST, "abc")).unwrap();
        sign_file(&key, &id, &file).unwrap();
        let error = verify_file(&file, &trusted).unwrap_err();
        assert!(
            format!("{error:#}").contains("no kotlin-lsp entry"),
            "{error:#}"
        );
    }

    #[test]
    fn test_bad_command_lines_print_the_usage() {
        for bad in [
            vec![],
            vec!["keygen"],
            vec!["sign", "key", "id"],
            vec!["verify", "file"],
            vec!["other"],
        ] {
            let error = run(&args(&bad)).unwrap_err();
            assert!(error.to_string().contains("usage:"), "{bad:?}");
        }
    }
}
