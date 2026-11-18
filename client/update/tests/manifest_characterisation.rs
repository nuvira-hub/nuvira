//! Characterisation tests (Phase 2 of the full review) of the update checks:
//! version comparison, manifest signatures, and `check_channel` / `download`
//! against a local HTTP server that answers what each test wants.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use nuvira_update::{CURRENT, Manifest, check_channel, download, newer};
use ring::signature::{Ed25519KeyPair, KeyPair};

fn key() -> Ed25519KeyPair {
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
    Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap()
}

fn public(k: &Ed25519KeyPair) -> [u8; 32] {
    k.public_key().as_ref().try_into().unwrap()
}

/// A local HTTP server answering every connection with `response`; returns the base
/// URL and the request lines it got.
fn serve(response: Vec<u8>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { return };
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") && s.read(&mut byte).unwrap_or(0) == 1 {
                head.push(byte[0]);
            }
            let line = String::from_utf8_lossy(&head).lines().next().unwrap_or_default().to_string();
            seen.lock().unwrap().push(line);
            let _ = s.write_all(&response);
        }
    });
    (format!("http://127.0.0.1:{port}/nuvira/update"), requests)
}

fn ok_with(body: &[u8]) -> Vec<u8> {
    let mut r = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
    r.extend_from_slice(body);
    r
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

/// A version newer than this build.
fn next_version() -> String {
    let mut parts: Vec<u64> = CURRENT.split('.').map(|p| p.parse().unwrap()).collect();
    parts[1] += 1;
    format!("{}.{}.0", parts[0], parts[1])
}

#[test]
fn version_comparison_edge_cases() {
    assert!(newer("0.0.10", "0.0.9"));
    assert!(newer("1.0.0.1", "1.0.0"), "a fourth part counts");
    assert!(!newer("", "0"));
    assert!(!newer("0", ""));
    // Parts that are not numbers count as 0.
    assert!(!newer("1.x.0", "1.0.0"));
    assert!(!newer("99999999999999999999.0", "0.0"), "beyond u64: 0");
    assert!(newer("1.0", "0.99999999999999999999"), "beyond u64 on the other side: 0");
}

#[test]
fn characterises_bug_pre_release_versions_compare_wrongly() {
    // KNOWN BUG (Phase 2): `newer` splits on '.' and parses each part as u64,
    // non-numbers being 0. A leading "v" makes the major 0, and the pre-release
    // suffixes of release candidates (the 1.0 is planned "via RC") compare wrongly:
    // "1.0.0-rc.1" = [1, 0, 0, 1] is NEWER than "1.0.0" = [1, 0, 0], so a device
    // on the RC would never be offered the final release; "1.0.0-rc1" = [1, 0, 0]
    // equals "1.0.0". Expected after fix: semver ordering (rc < release).
    assert!(!newer("v1.0.0", "0.28.0"));
    assert!(newer("1.0.0-rc.1", "1.0.0"));
    assert!(!newer("1.0.0", "1.0.0-rc.1"));
    assert!(!newer("1.0.0", "1.0.0-rc1"));
    assert!(!newer("1.0.0-rc.2", "1.0.0-rc.10"));
    assert!(newer("1.0.0-rc.2", "1.0.0-rc.1"));
}

#[test]
fn signature_edge_cases() {
    let k = key();
    let m = Manifest::sign(&k, "0.2.0", "windows-x64", b"program");
    assert_eq!(m.signature.len(), 128);
    // Upper-case hex is accepted.
    let upper = Manifest { signature: m.signature.to_uppercase(), ..m.clone() };
    upper.verify(&public(&k)).unwrap();
    for (signature, error) in [
        ("zz".repeat(64), "signature is not hexadecimal"),
        (m.signature[..127].to_string(), "signature is not hexadecimal"),
        (m.signature[..126].to_string(), "the update signature is not valid"),
        (String::new(), "the update signature is not valid"),
    ] {
        let t = Manifest { signature, ..m.clone() };
        assert_eq!(t.verify(&public(&k)).unwrap_err().to_string(), error);
    }
    // A public key of the wrong length, or no keys at all.
    assert!(m.verify(&public(&k)[..31]).is_err());
    assert!(m.verify_any(&[]).is_err());
    // The sha256 text is signed as written: upper case is another manifest.
    let t = Manifest { sha256: m.sha256.to_uppercase(), ..m.clone() };
    assert!(t.verify(&public(&k)).is_err());
}

#[test]
fn manifest_json() {
    let k = key();
    let m = Manifest::sign(&k, "0.2.0", "linux-x64", b"p");
    let json = serde_json::to_value(&m).unwrap();
    let keys: Vec<&str> = json.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys.len(), 5);
    for k in ["version", "platform", "size", "sha256", "signature"] {
        assert!(keys.contains(&k), "{k}");
    }
    // Unknown fields are ignored; a missing one is an error.
    let mut extra = json.clone();
    extra["notes"] = "hello".into();
    assert_eq!(serde_json::from_value::<Manifest>(extra).unwrap(), m);
    let mut missing = json.clone();
    missing.as_object_mut().unwrap().remove("size");
    assert!(serde_json::from_value::<Manifest>(missing).is_err());
}

#[test]
fn check_channel_answers() {
    let k = key();
    let keys = [public(&k)];
    let rt = rt();
    let check = |response: Vec<u8>| {
        let (base, requests) = serve(response);
        let result = rt.block_on(check_channel(&base, &keys, "linux-x64"));
        (result, requests.lock().unwrap().clone())
    };
    let newer_one = Manifest::sign(&k, &next_version(), "linux-x64", b"program");
    let json = |m: &Manifest| serde_json::to_vec(m).unwrap();

    let (result, requests) = check(ok_with(&json(&newer_one)));
    assert_eq!(result.unwrap(), Some(newer_one.clone()));
    assert_eq!(requests, ["GET /nuvira/update/linux-x64.json HTTP/1.1"]);

    // The same version as this build: nothing to do.
    let same = Manifest::sign(&k, CURRENT, "linux-x64", b"program");
    assert_eq!(check(ok_with(&json(&same))).0.unwrap(), None);
    // 404: no update published.
    assert_eq!(check(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec()).0.unwrap(), None);

    let err = |response: Vec<u8>| check(response).0.unwrap_err().to_string();
    let other_platform = Manifest::sign(&k, &next_version(), "windows-x64", b"program");
    assert!(err(ok_with(&json(&other_platform))).ends_with("is for windows-x64"));
    let other_key = Manifest::sign(&key(), &next_version(), "linux-x64", b"program");
    assert_eq!(err(ok_with(&json(&other_key))), "the update signature is not valid");
    assert!(err(ok_with(b"not json")).ends_with("linux-x64.json is not valid"));
    assert!(err(b"HTTP/1.1 500 Oops\r\n\r\n".to_vec()).ends_with("HTTP 500"));
    assert_eq!(err(b"garbage\r\n\r\n".to_vec()), "not an HTTP response");
    assert!(
        err(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec())
            .ends_with("chunked responses are not supported")
    );
    // The manifest limit, 64 KiB: announced, or found without Content-Length.
    assert!(
        err(b"HTTP/1.1 200 OK\r\nContent-Length: 65537\r\n\r\n".to_vec())
            .ends_with("65537 bytes, more than the 65536 expected")
    );
    let mut long = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
    long.extend(vec![b' '; 70_000]);
    assert!(err(long).ends_with("more than the 65536 bytes expected"));
    // Headers longer than 16 KiB.
    let mut head = b"HTTP/1.1 200 OK\r\nX: ".to_vec();
    head.extend(vec![b'a'; 17_000]);
    assert!(err(head).ends_with("headers too long"));
    // Closed before the announced length.
    assert!(
        err(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc".to_vec())
            .contains("connection closed after 3")
    );
    // Header names in any case, Transfer-Encoding: identity accepted.
    let body = json(&newer_one);
    let mut r =
        format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\ntransfer-encoding: Identity\r\n\r\n", body.len())
            .into_bytes();
    r.extend(&body);
    assert_eq!(check(r).0.unwrap(), Some(newer_one));
}

#[test]
fn download_paths_and_limits() {
    let k = key();
    let rt = rt();
    let dir = std::env::temp_dir().join(format!("nuvira-p2-update-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dest = dir.join("new.exe");

    // The version is used in the URL path as signed, without any check.
    let program = b"program".to_vec();
    let odd = Manifest::sign(&k, "../../x", "windows-x64", &program);
    let (base, requests) = serve(ok_with(&program));
    rt.block_on(download(&base, &odd, &dest)).unwrap();
    assert_eq!(
        requests.lock().unwrap().as_slice(),
        ["GET /nuvira/update/windows-x64/nuvira-../../x.exe HTTP/1.1"]
    );
    assert_eq!(std::fs::read(&dest).unwrap(), program);
    std::fs::remove_file(&dest).unwrap();

    // An empty program.
    let empty = Manifest::sign(&k, "1.0.0", "linux-x64", b"");
    let (base, _) = serve(ok_with(b""));
    rt.block_on(download(&base, &empty, &dest)).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), b"");
    std::fs::remove_file(&dest).unwrap();

    // More than the manifest's size is refused, and nothing is left.
    let (base, _) = serve(ok_with(b"program and more"));
    let err = rt.block_on(download(&base, &Manifest::sign(&k, "1.0.0", "linux-x64", &program), &dest));
    assert!(err.unwrap_err().to_string().contains("more than the 7 expected"));
    assert!(!dest.exists() && !dest.with_extension("part").exists());

    // The manifest's size above 256 MiB: the limit is 256 MiB.
    let mut big = Manifest::sign(&k, "1.0.0", "linux-x64", &program);
    big.size = 300 << 20;
    let (base, _) =
        serve(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", 300u64 << 20).into_bytes());
    let err = rt.block_on(download(&base, &big, &dest)).unwrap_err().to_string();
    assert!(err.contains(&format!("more than the {} expected", 256u64 << 20)), "{err}");

    // Not published: an error, not "nothing to do".
    let (base, _) = serve(b"HTTP/1.1 404 Not Found\r\n\r\n".to_vec());
    let err = rt.block_on(download(&base, &empty, &dest)).unwrap_err().to_string();
    assert_eq!(err, "the program is not on the server");
    let _ = std::fs::remove_dir_all(&dir);
}
