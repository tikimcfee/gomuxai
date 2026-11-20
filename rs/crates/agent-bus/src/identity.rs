//! Bus identity, pinned TLS certificate, and peer trust (`internal/natsbus/identity.go`).

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use agent_base::gostd::os::PathError;
use agent_base::gostd::strconv::quote;
use async_nats::rustls::pki_types::pem::PemObject;
use async_nats::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use data_encoding::HEXLOWER;
use nkeys::{KeyPair, KeyPairType};
use rcgen::{
    CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, KeyUsagePurpose,
    PublicKeyData, SanType,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::contract::{null_default, validate_host};

const HOST_NAME_FILE: &str = "host";
const HOST_SEED_FILE: &str = "host.nk";
const LOCAL_SEED_FILE: &str = "local.nk";
const TLS_KEY_FILE: &str = "tls.key";
const TLS_CERT_FILE: &str = "tls.crt";
const PEERS_DIR: &str = "peers";
const PREV_HOSTS_FILE: &str = "host.previous";

// PKCS#8 v1 prefix for ECDSA P-256 (prime256v1):
//   INTEGER 0
//   SEQUENCE { OID id-ecPublicKey (1.2.840.10045.2.1), OID prime256v1 (1.2.840.10045.3.1.7) }
const PKCS8_PREFIX_ECDSA_P256: &[u8] = b"\x02\x01\x00\
    \x30\x13\
    \x06\x07\x2a\x86\x48\xce\x3d\x02\x01\
    \x06\x08\x2a\x86\x48\xce\x3d\x03\x01\x07";

/// Sentinel error meaning `agent bus init` hasn't been run on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("no bus identity on this host (run `agent bus init`)")]
pub struct ErrNoIdentity;

/// Go's `errors.Is(err, natsbus.ErrNoIdentity)`.
pub fn is_no_identity(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|e| e.downcast_ref::<ErrNoIdentity>().is_some())
}

/// `trust_peer` refusing a bundle whose key or certificate pin another trusted
/// peer already has.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "the bundle for {host} has the keys of trusted peer {peer}; if {peer} was renamed to {host}, re-trust it with --renames {peer}"
)]
pub struct SameKeysError {
    pub host: String,
    pub peer: String,
}

/// A host's bus credentials.
#[derive(Debug, Clone)]
pub struct Identity {
    pub host: String,
    pub host_key: KeyPair,
    pub local_key: KeyPair,
    pub cert_der: Vec<u8>,
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
    /// Hex SHA-256 of the certificate's `SubjectPublicKeyInfo`.
    pub cert_sha: String,
    pub(crate) host_seed: Vec<u8>,
    pub(crate) local_seed: String,
}

impl Identity {
    /// Returns what peers need to trust this host. `url` is where this host
    /// accepts leaf links, or `""` if it only connects out.
    pub fn public_bundle(&self, url: &str) -> PeerBundle {
        PeerBundle {
            host: self.host.clone(),
            nkey: self.host_key.public_key(),
            cert_sha: self.cert_sha.clone(),
            url: url.to_string(),
            aka: Vec::new(),
        }
    }

    /// The public nkey local clients authenticate as.
    pub fn local_public_key(&self) -> String {
        self.local_key.public_key()
    }

    /// The public nkey this host authenticates leaf links with.
    pub fn host_public_key(&self) -> String {
        self.host_key.public_key()
    }

    /// Raw nkey seed bytes for the host's leaf-link keypair.
    pub fn host_seed(&self) -> &[u8] {
        &self.host_seed
    }
}

/// The public half of a host's identity: everything another host needs to trust
/// it, and nothing secret.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerBundle {
    #[serde(default, deserialize_with = "null_default")]
    pub host: String,
    #[serde(default, deserialize_with = "null_default")]
    pub nkey: String,
    #[serde(default, deserialize_with = "null_default", rename = "cert_sha256")]
    pub cert_sha: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub url: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub aka: Vec<String>,
}

impl PeerBundle {
    /// Checks a bundle's fields before it is trusted.
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_host(&self.host)?;
        if !is_valid_public_user_key(&self.nkey) {
            anyhow::bail!("peer {}: invalid public nkey", self.host);
        }
        if self.cert_sha.len() != 64 || !self.cert_sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            anyhow::bail!("peer {}: cert_sha256 must be 64 hex characters", self.host);
        }
        if !self.url.is_empty() && !self.url.starts_with("tls://") {
            anyhow::bail!("peer {}: url must use tls://", self.host);
        }
        for name in &self.aka {
            validate_host(name)
                .map_err(|e| anyhow::anyhow!("peer {}: former name: {e}", self.host))?;
        }
        Ok(())
    }

    /// Reports whether `name` is this peer's name or one it had before,
    /// ignoring case.
    pub fn names(&self, name: &str) -> bool {
        eq_fold(&self.host, name) || self.aka.iter().any(|n| eq_fold(n, name))
    }
}

fn eq_fold(a: &str, b: &str) -> bool {
    if a.is_ascii() && b.is_ascii() {
        a.eq_ignore_ascii_case(b)
    } else {
        a.to_lowercase() == b.to_lowercase()
    }
}

fn is_valid_public_user_key(s: &str) -> bool {
    s.starts_with('U')
        && KeyPair::from_public_key(s)
            .map(|kp| kp.key_pair_type() == KeyPairType::User)
            .unwrap_or(false)
}

/// Creates a new identity for `host` in `dir`. Refuses to replace an existing
/// one.
pub fn init_identity(dir: impl AsRef<Path>, host: &str) -> anyhow::Result<Identity> {
    let dir = dir.as_ref();
    validate_host(host)?;
    if dir.join(HOST_SEED_FILE).exists() {
        anyhow::bail!("bus identity already exists in {}", dir.display());
    }

    let host_key = KeyPair::new_user();
    let local_key = KeyPair::new_user();
    let tls_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|e| anyhow::anyhow!("generating TLS key: {e}"))?;
    let (cert_pem, key_pem) = self_signed_cert(host, &tls_key)?;

    let host_seed = host_key
        .seed()
        .map_err(|e| anyhow::anyhow!("creating host nkey: {e}"))?;
    let local_seed = local_key
        .seed()
        .map_err(|e| anyhow::anyhow!("creating local nkey: {e}"))?;

    let peers_path = dir.join(PEERS_DIR);
    agent_base::gostd::os::mkdir_all(&peers_path.to_string_lossy(), 0o700)
        .map_err(|e| anyhow::anyhow!("creating bus dir: {e}"))?;

    let host_line = format!("{host}\n");
    let files: &[(&str, &[u8], u32)] = &[
        (HOST_NAME_FILE, host_line.as_bytes(), 0o644),
        (TLS_CERT_FILE, &cert_pem, 0o644),
        (TLS_KEY_FILE, &key_pem, 0o600),
        (LOCAL_SEED_FILE, local_seed.as_bytes(), 0o600),
        (HOST_SEED_FILE, host_seed.as_bytes(), 0o600), // last: marks a complete identity
    ];
    for &(name, data, mode) in files {
        write_file_exclusive(&dir.join(name), data, mode)?;
    }
    load_identity(dir, host)
}

/// Reads the identity in `dir`, checking secret file permissions.
pub fn load_identity(dir: impl AsRef<Path>, host: &str) -> anyhow::Result<Identity> {
    let dir = dir.as_ref();
    let read = |name: &str, secret: bool| -> anyhow::Result<Vec<u8>> {
        let path = dir.join(name);
        let path_str = path.to_string_lossy().into_owned();
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(ErrNoIdentity.into()),
            Err(e) => return Err(PathError::new("stat", &path_str, e).into()),
        };
        let perm = meta.permissions().mode() & 0o777;
        if secret && (perm & 0o077) != 0 {
            anyhow::bail!("{path_str} is readable by other users (mode {perm:o}); run chmod 600");
        }
        Ok(agent_base::gostd::os::read_file(&path_str)?)
    };

    let name_bytes = read(HOST_NAME_FILE, false)?;
    let recorded = String::from_utf8_lossy(&name_bytes).trim().to_string();
    if recorded != host {
        if previous_hosts(dir).iter().any(|n| eq_fold(n, host)) {
            anyhow::bail!(
                "bus identity in {} was renamed from {} to {} (agent bus rename): run as {}",
                dir.display(),
                quote(host),
                quote(&recorded),
                recorded
            );
        }
        anyhow::bail!(
            "bus identity in {} is for host {}, not {} (set AGENT_HOST={}, or init a new identity)",
            dir.display(),
            quote(&recorded),
            quote(host),
            recorded
        );
    }

    let host_seed = read(HOST_SEED_FILE, true)?;
    let local_seed_bytes = read(LOCAL_SEED_FILE, true)?;
    let key_pem = read(TLS_KEY_FILE, true)?;
    let cert_pem = read(TLS_CERT_FILE, false)?;

    let host_seed_str = std::str::from_utf8(&host_seed)
        .map_err(|e| anyhow::anyhow!("reading {HOST_SEED_FILE}: {e}"))?
        .trim();
    let host_key = KeyPair::from_seed(host_seed_str)
        .map_err(|e| anyhow::anyhow!("reading {HOST_SEED_FILE}: {e}"))?;

    let local_seed_str = std::str::from_utf8(&local_seed_bytes)
        .map_err(|e| anyhow::anyhow!("reading {LOCAL_SEED_FILE}: {e}"))?
        .trim()
        .to_string();
    let local_key = KeyPair::from_seed(&local_seed_str)
        .map_err(|e| anyhow::anyhow!("reading {LOCAL_SEED_FILE}: {e}"))?;

    let cert_der = CertificateDer::from_pem_slice(&cert_pem)
        .map_err(|e| anyhow::anyhow!("loading TLS key pair: {e}"))?
        .into_owned();
    let cert_sha = spki_sha256(cert_der.as_ref())
        .map_err(|e| anyhow::anyhow!("parsing TLS certificate: {e}"))?;

    let parsed_key = parse_tls_private_key(&key_pem)
        .map_err(|e| anyhow::anyhow!("loading TLS key pair: {e}"))?;
    let key_spki_sha = sha256_hex(&parsed_key.subject_public_key_info());
    if key_spki_sha != cert_sha {
        anyhow::bail!("loading TLS key pair: private key does not match public key");
    }

    Ok(Identity {
        host: host.to_string(),
        host_key,
        local_key,
        cert_der: cert_der.to_vec(),
        cert_pem,
        key_pem,
        cert_sha,
        host_seed: host_seed_str.as_bytes().to_vec(),
        local_seed: local_seed_str,
    })
}

/// Installs a peer's public bundle. Re-trusting a host replaces its bundle while
/// preserving its former names if `b.aka` is empty.
pub fn trust_peer(dir: impl AsRef<Path>, mut b: PeerBundle) -> anyhow::Result<()> {
    let dir = dir.as_ref();
    b.validate()?;
    let peers = load_peers(dir)?;
    let mut replaced: Option<PeerBundle> = None;
    for p in peers {
        if eq_fold(&p.host, &b.host) {
            replaced = Some(p);
            continue;
        }
        if p.nkey == b.nkey || p.cert_sha == b.cert_sha {
            return Err(SameKeysError {
                host: b.host,
                peer: p.host,
            }
            .into());
        }
    }
    if let Some(ref prev) = replaced
        && b.aka.is_empty()
    {
        b.aka = prev.aka.clone();
    }
    write_peer(dir, &b, replaced.as_ref())
}

/// Replaces the trusted peer `old` with `b`, the same machine under a new name.
/// Returns `(installed, replaced)`.
pub fn rename_peer(
    dir: impl AsRef<Path>,
    old: &str,
    mut b: PeerBundle,
) -> anyhow::Result<(PeerBundle, PeerBundle)> {
    let dir = dir.as_ref();
    b.validate()?;
    let peers = load_peers(dir)?;
    let mut prev: Option<PeerBundle> = None;
    for p in &peers {
        if eq_fold(&p.host, old) {
            prev = Some(p.clone());
        }
    }
    let Some(prev) = prev else {
        anyhow::bail!(
            "no trusted peer {} to rename ({})",
            quote(old),
            trusted_list(&peers)
        );
    };
    if b.nkey != prev.nkey || b.cert_sha != prev.cert_sha {
        anyhow::bail!(
            "the bundle for {} doesn't have trusted peer {}'s keys (key {}, pin {}; {} has key {}, pin {}), so it isn't {} renamed: trust it without --renames, and agent bus untrust {} if that host is gone",
            b.host,
            prev.host,
            b.nkey,
            b.cert_sha,
            prev.host,
            prev.nkey,
            prev.cert_sha,
            prev.host,
            prev.host
        );
    }
    for p in &peers {
        if p.host != prev.host && eq_fold(&p.host, &b.host) {
            anyhow::bail!(
                "another trusted peer is already named {}: agent bus untrust {} first, if it's gone",
                p.host,
                p.host
            );
        }
    }
    let mut aka: Vec<String> = Vec::new();
    for n in prev.aka.iter().chain(std::iter::once(&prev.host)) {
        if !eq_fold(n, &b.host) && !aka.iter().any(|a| eq_fold(a, n)) {
            aka.push(n.clone());
        }
    }
    b.aka = aka;
    write_peer(dir, &b, Some(&prev))?;
    Ok((b, prev))
}

/// Removes the trusted peer named `host` (ignoring case) and returns its bundle.
pub fn untrust_peer(dir: impl AsRef<Path>, host: &str) -> anyhow::Result<PeerBundle> {
    let dir = dir.as_ref();
    let peers = load_peers(dir)?;
    for p in &peers {
        if eq_fold(&p.host, host) {
            let path = peer_path(dir, &p.host);
            std::fs::remove_file(&path)
                .map_err(|e| PathError::new("remove", &path.to_string_lossy(), e))?;
            return Ok(p.clone());
        }
    }
    anyhow::bail!("no trusted peer {} ({})", quote(host), trusted_list(&peers));
}

fn trusted_list(peers: &[PeerBundle]) -> String {
    if peers.is_empty() {
        return "this host trusts no peers".to_string();
    }
    let mut hosts: Vec<&str> = peers.iter().map(|p| p.host.as_str()).collect();
    hosts.sort_unstable();
    format!("trusted: {}", hosts.join(", "))
}

fn peer_path(dir: &Path, host: &str) -> std::path::PathBuf {
    dir.join(PEERS_DIR).join(format!("{host}.json"))
}

fn write_peer(dir: &Path, b: &PeerBundle, prev: Option<&PeerBundle>) -> anyhow::Result<()> {
    let mut data = serde_json::to_vec_pretty(b)?;
    data.push(b'\n');
    let path = peer_path(dir, &b.host);
    let peers_dir = dir.join(PEERS_DIR);
    agent_base::gostd::os::mkdir_all(&peers_dir.to_string_lossy(), 0o700)?;
    let tmp = peer_path(dir, &format!("{}.json_tmp", b.host))
        .with_file_name(format!("{}.json.tmp", b.host));
    write_file_mode(&tmp, &data, 0o644)?;
    if let Some(prev) = prev
        && prev.host != b.host
    {
        let old_path = peer_path(dir, &prev.host);
        if let Err(e) = std::fs::remove_file(&old_path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            let _ = std::fs::remove_file(&tmp);
            return Err(PathError::new("remove", &old_path.to_string_lossy(), e).into());
        }
    }
    std::fs::rename(&tmp, &path).map_err(|e| agent_base::gostd::os::LinkError {
        op: "rename".to_string(),
        old: tmp.to_string_lossy().into_owned(),
        new: path.to_string_lossy().into_owned(),
        err: e,
    })?;
    Ok(())
}

/// Reads every trusted peer bundle in `dir`.
pub fn load_peers(dir: impl AsRef<Path>) -> anyhow::Result<Vec<PeerBundle>> {
    let peers_dir = dir.as_ref().join(PEERS_DIR);
    let entries = match std::fs::read_dir(&peers_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(PathError::new("open", &peers_dir.to_string_lossy(), e).into());
        }
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|e| PathError::new("readdir", &peers_dir.to_string_lossy(), e))?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("json") {
            paths.push(path);
        }
    }
    paths.sort();

    let mut peers = Vec::with_capacity(paths.len());
    for path in paths {
        let path_str = path.to_string_lossy().into_owned();
        let data = agent_base::gostd::os::read_file(&path_str)?;
        let b: PeerBundle = serde_json::from_slice(&data)
            .map_err(|e| anyhow::anyhow!("reading {path_str}: {e}"))?;
        b.validate()
            .map_err(|e| anyhow::anyhow!("{path_str}: {e}"))?;
        let expected_name = format!("{}.json", b.host);
        if path.file_name().and_then(|n| n.to_str()) != Some(expected_name.as_str()) {
            anyhow::bail!("{path_str}: bundle is for host {}", quote(&b.host));
        }
        peers.push(b);
    }
    Ok(peers)
}

/// Lists the names the identity in `dir` had before (`agent bus rename`),
/// oldest first.
pub fn previous_hosts(dir: impl AsRef<Path>) -> Vec<String> {
    let path = dir.as_ref().join(PREV_HOSTS_FILE);
    let Ok(data) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    data.lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// Says why the identity in `dir`, now `old`'s, can't be renamed to `new`, or
/// `Ok(())` if it can.
pub fn check_rename(dir: impl AsRef<Path>, old: &str, new: &str) -> anyhow::Result<()> {
    let dir = dir.as_ref();
    validate_host(new)?;
    if new == old {
        anyhow::bail!("this host is already named {old}");
    }
    load_identity(dir, old)?;
    let peers = load_peers(dir)?;
    for p in &peers {
        if p.names(new) {
            anyhow::bail!(
                "trusted peer {} is (or was) named {}; pick another name",
                p.host,
                new
            );
        }
    }
    Ok(())
}

/// Binds the identity in `dir`, now `old`'s, to `new`, keeping all keys and the
/// certificate's SPKI pin.
pub fn rename_identity(dir: impl AsRef<Path>, old: &str, new: &str) -> anyhow::Result<Identity> {
    let dir = dir.as_ref();
    check_rename(dir, old, new)?;

    let key_path = dir.join(TLS_KEY_FILE);
    let key_pem = agent_base::gostd::os::read_file(&key_path.to_string_lossy())?;
    let tls_key = parse_tls_private_key(&key_pem)
        .map_err(|e| anyhow::anyhow!("reading {TLS_KEY_FILE}: {e}"))?;
    let (cert_pem, _) = self_signed_cert(new, &tls_key)?;

    let mut prev: Vec<String> = previous_hosts(dir)
        .into_iter()
        .filter(|n| !eq_fold(n, new) && !eq_fold(n, old))
        .collect();
    prev.push(old.to_string());
    let prev_bytes = format!("{}\n", prev.join("\n")).into_bytes();
    let host_bytes = format!("{new}\n").into_bytes();

    for (name, data) in [
        (TLS_CERT_FILE, cert_pem.as_slice()),
        (PREV_HOSTS_FILE, prev_bytes.as_slice()),
        (HOST_NAME_FILE, host_bytes.as_slice()), // last: commits the rename
    ] {
        write_file_atomic(&dir.join(name), data, 0o644)?;
    }
    load_identity(dir, new)
}

/// Returns `host` as the bus spells it: `self_host` or a trusted peer's name
/// when `host` matches one ignoring case, or a name either had before it was
/// renamed; any other host as given (`internal/cli/remote.go`).
pub fn canonical_host(dir: impl AsRef<Path>, self_host: &str, host: &str) -> String {
    if host.is_empty() || host == self_host {
        return host.to_string();
    }
    let dir = dir.as_ref();
    let same = |name: &str| eq_fold(host, name);
    if same(self_host) || previous_hosts(dir).iter().any(|n| same(n)) {
        return self_host.to_string();
    }
    let peers = load_peers(dir).unwrap_or_default();
    for p in &peers {
        if same(&p.host) {
            return p.host.clone();
        }
    }
    for p in &peers {
        if p.names(host) {
            return p.host.clone();
        }
    }
    host.to_string()
}

/// Computes the hex SHA-256 of the X.509 certificate's `RawSubjectPublicKeyInfo`
/// (`spkiSHA256` in Go).
pub fn spki_sha256(cert_der: &[u8]) -> anyhow::Result<String> {
    let (_, cert) = x509_parser::parse_x509_certificate(cert_der)
        .map_err(|e| anyhow::anyhow!("invalid X.509 certificate: {e}"))?;
    Ok(sha256_hex(cert.public_key().raw))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    HEXLOWER.encode(&digest)
}

fn self_signed_cert(host: &str, key: &rcgen::KeyPair) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, format!("gomux bus {host}"));
    params.distinguished_name = dn;
    let dns_name = host
        .to_string()
        .try_into()
        .map_err(|e| anyhow::anyhow!("creating TLS certificate: {e}"))?;
    params.subject_alt_names = vec![SanType::DnsName(dns_name)];

    let now = std::time::SystemTime::now();
    let hour = std::time::Duration::from_secs(3600);
    let ten_years = std::time::Duration::from_secs(10 * 365 * 24 * 3600);
    params.not_before = (now - hour).into();
    params.not_after = (now + ten_years).into();
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];

    let cert = params
        .self_signed(key)
        .map_err(|e| anyhow::anyhow!("creating TLS certificate: {e}"))?;
    let cert_pem = cert.pem().replace("\r\n", "\n").into_bytes();
    let key_pem = serialize_sec1_p256_pem(key)?;
    Ok((cert_pem, key_pem))
}

/// Encodes an ECDSA P-256 `rcgen::KeyPair` as an RFC 5915 SEC 1
/// `-----BEGIN EC PRIVATE KEY-----` PEM block matching Go's
/// `x509.MarshalECPrivateKey`, so Go's `x509.ParseECPrivateKey` (used by
/// `agent bus rename`) and `tls.X509KeyPair` both accept it.
fn serialize_sec1_p256_pem(key: &rcgen::KeyPair) -> anyhow::Result<Vec<u8>> {
    let pkcs8 = key.serialize_der();
    let scalar = extract_p256_scalar_from_pkcs8(&pkcs8)?;
    let pubkey_raw = key.public_key_raw();

    // ECPrivateKey ::= SEQUENCE {
    //   version        INTEGER (1),
    //   privateKey     OCTET STRING (32 bytes),
    //   parameters [0] EXPLICIT OBJECT IDENTIFIER (prime256v1 = 1.2.840.10045.3.1.7),
    //   publicKey  [1] EXPLICIT BIT STRING (0x00 || uncompressed point)
    // }
    const VERSION_1: &[u8] = &[0x02, 0x01, 0x01];
    const PARAMS_P256: &[u8] = &[
        0xa0, 0x0a, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07,
    ];

    let priv_octet = der_tlv(0x04, scalar);
    let mut bit_string_payload = Vec::with_capacity(1 + pubkey_raw.len());
    bit_string_payload.push(0x00); // 0 unused bits
    bit_string_payload.extend_from_slice(pubkey_raw);
    let pub_bit_string = der_tlv(0x03, &bit_string_payload);
    let pub_explicit = der_tlv(0xa1, &pub_bit_string);

    let mut body = Vec::with_capacity(
        VERSION_1.len() + priv_octet.len() + PARAMS_P256.len() + pub_explicit.len(),
    );
    body.extend_from_slice(VERSION_1);
    body.extend_from_slice(&priv_octet);
    body.extend_from_slice(PARAMS_P256);
    body.extend_from_slice(&pub_explicit);
    let sec1_der = der_tlv(0x30, &body);

    let b64 = data_encoding::BASE64.encode(&sec1_der);
    let mut pem = String::from("-----BEGIN EC PRIVATE KEY-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(chunk).unwrap());
        pem.push('\n');
    }
    pem.push_str("-----END EC PRIVATE KEY-----\n");
    Ok(pem.into_bytes())
}

fn extract_p256_scalar_from_pkcs8(pkcs8: &[u8]) -> anyhow::Result<&[u8]> {
    let (tag, outer, rest) = read_der_tlv(pkcs8)?;
    if tag != 0x30 || !rest.is_empty() {
        anyhow::bail!("unexpected PKCS#8 outer structure");
    }
    let (vtag, _, after_ver) = read_der_tlv(outer)?;
    if vtag != 0x02 {
        anyhow::bail!("unexpected PKCS#8 version tag");
    }
    let (atag, _, after_alg) = read_der_tlv(after_ver)?;
    if atag != 0x30 {
        anyhow::bail!("unexpected PKCS#8 algorithm tag");
    }
    let (ptag, priv_octet, _) = read_der_tlv(after_alg)?;
    if ptag != 0x04 {
        anyhow::bail!("unexpected PKCS#8 privateKey tag");
    }
    let (stag, sec1_seq, _) = read_der_tlv(priv_octet)?;
    if stag != 0x30 {
        anyhow::bail!("unexpected inner SEC 1 sequence tag");
    }
    let (svtag, _, after_sver) = read_der_tlv(sec1_seq)?;
    if svtag != 0x02 {
        anyhow::bail!("unexpected inner SEC 1 version tag");
    }
    let (ktag, scalar, _) = read_der_tlv(after_sver)?;
    if ktag != 0x04 || scalar.len() != 32 {
        anyhow::bail!("unexpected P-256 private scalar length {}", scalar.len());
    }
    Ok(scalar)
}

fn read_der_tlv(input: &[u8]) -> anyhow::Result<(u8, &[u8], &[u8])> {
    if input.len() < 2 {
        anyhow::bail!("truncated DER TLV");
    }
    let tag = input[0];
    let len_byte = input[1];
    let (len, hdr_len) = if (len_byte & 0x80) == 0 {
        (len_byte as usize, 2usize)
    } else {
        let num_octets = (len_byte & 0x7f) as usize;
        if num_octets == 0 || num_octets > 3 || input.len() < 2 + num_octets {
            anyhow::bail!("invalid DER length");
        }
        let mut l = 0usize;
        for &b in &input[2..2 + num_octets] {
            l = (l << 8) | (b as usize);
        }
        (l, 2 + num_octets)
    };
    if input.len() < hdr_len + len {
        anyhow::bail!("truncated DER value");
    }
    Ok((tag, &input[hdr_len..hdr_len + len], &input[hdr_len + len..]))
}

/// Parses a PEM private key written by either Rust (`PRIVATE KEY`, PKCS#8) or
/// Go (`EC PRIVATE KEY`, SEC 1 P-256).
fn parse_tls_private_key(key_pem: &[u8]) -> anyhow::Result<rcgen::KeyPair> {
    let key_der =
        PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| anyhow::anyhow!("no PEM key: {e}"))?;
    match key_der {
        PrivateKeyDer::Pkcs8(pkcs8) => {
            rcgen::KeyPair::from_pkcs8_der_and_sign_algo(&pkcs8, &rcgen::PKCS_ECDSA_P256_SHA256)
                .map_err(|e| anyhow::anyhow!("{e}"))
        }
        PrivateKeyDer::Sec1(sec1) => {
            let pkcs8_bytes = sec1_p256_to_pkcs8(sec1.secret_sec1_der());
            let pkcs8 = PrivatePkcs8KeyDer::from(pkcs8_bytes);
            rcgen::KeyPair::from_pkcs8_der_and_sign_algo(&pkcs8, &rcgen::PKCS_ECDSA_P256_SHA256)
                .map_err(|e| anyhow::anyhow!("{e}"))
        }
        _ => anyhow::bail!("unsupported private key format"),
    }
}

/// Wraps a SEC 1 `ECPrivateKey` DER (as Go's `x509.MarshalECPrivateKey` writes)
/// into a PKCS#8 v1 `PrivateKeyInfo` DER for P-256.
fn sec1_p256_to_pkcs8(sec1_der: &[u8]) -> Vec<u8> {
    let octet_string = der_tlv(0x04, sec1_der);
    let mut seq_body = Vec::with_capacity(PKCS8_PREFIX_ECDSA_P256.len() + octet_string.len());
    seq_body.extend_from_slice(PKCS8_PREFIX_ECDSA_P256);
    seq_body.extend_from_slice(&octet_string);
    der_tlv(0x30, &seq_body)
}

fn der_tlv(tag: u8, payload: &[u8]) -> Vec<u8> {
    let len = payload.len();
    let mut out = Vec::with_capacity(4 + len);
    out.push(tag);
    if len < 0x80 {
        out.push(len as u8);
    } else if len <= 0xff {
        out.push(0x81);
        out.push(len as u8);
    } else {
        out.push(0x82);
        out.push((len >> 8) as u8);
        out.push((len & 0xff) as u8);
    }
    out.extend_from_slice(payload);
    out
}

fn write_file_exclusive(path: &Path, data: &[u8], mode: u32) -> anyhow::Result<()> {
    let path_str = path.to_string_lossy().into_owned();
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .map_err(|e| {
            anyhow::anyhow!(
                "writing {path_str}: {}",
                PathError::new("open", &path_str, e)
            )
        })?;
    f.set_permissions(std::fs::Permissions::from_mode(mode))
        .map_err(|e| {
            anyhow::anyhow!(
                "writing {path_str}: {}",
                PathError::new("chmod", &path_str, e)
            )
        })?;
    f.write_all(data).map_err(|e| {
        anyhow::anyhow!(
            "writing {path_str}: {}",
            PathError::new("write", &path_str, e)
        )
    })?;
    Ok(())
}

fn write_file_mode(path: &Path, data: &[u8], mode: u32) -> anyhow::Result<()> {
    let path_str = path.to_string_lossy().into_owned();
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)
        .map_err(|e| PathError::new("open", &path_str, e))?;
    f.set_permissions(std::fs::Permissions::from_mode(mode))
        .map_err(|e| PathError::new("chmod", &path_str, e))?;
    f.write_all(data)
        .map_err(|e| PathError::new("write", &path_str, e))?;
    Ok(())
}

fn write_file_atomic(path: &Path, data: &[u8], mode: u32) -> anyhow::Result<()> {
    let path_str = path.to_string_lossy().into_owned();
    let tmp = std::path::PathBuf::from(format!("{path_str}.tmp"));
    if let Err(e) = write_file_mode(&tmp, data, mode) {
        anyhow::bail!("writing {path_str}: {e}");
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        let link_err = agent_base::gostd::os::LinkError {
            op: "rename".to_string(),
            old: tmp.to_string_lossy().into_owned(),
            new: path_str.clone(),
            err: e,
        };
        anyhow::bail!("writing {path_str}: {link_err}");
    }
    Ok(())
}
