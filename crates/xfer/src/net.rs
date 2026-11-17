//! TLS transport: listener, requests, resumable downloads.

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ddc_core::config::XferConfig;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::sign::{CertifiedKey, SingleCertAndKey};
use rustls::{
    CertificateError, ClientConfig, ClientConnection, DigitallySignedStruct, DistinguishedName,
    ServerConfig, ServerConnection, SignatureScheme, StreamOwned,
};

use crate::clip::RemoteFiles;
use crate::offer::LocalOffer;
use crate::proto::{
    read_msg, safe_components, validate_entries, write_msg, Entry, Request, Response, MAGIC,
};
use crate::trust::{self, Trust, CERT_FILE};
use crate::{Direction, Error, Event, Progress, RemoteOffer};

const CHUNK: usize = 256 * 1024;
const PROGRESS_EVERY: Duration = Duration::from_millis(250);
const HELLO_EVERY: Duration = Duration::from_secs(30);
/// How many of our recent offers stay downloadable.
const KEEP_OFFERS: usize = 4;
/// Name the client asks for; the verifier ignores it (trust is by fingerprint).
const SERVER_NAME: &str = "monswitch-xfer";

type ClientStream = StreamOwned<ClientConnection, TcpStream>;

// ---- TLS ------------------------------------------------------------------------

/// Accepts exactly the certificates whose fingerprint Deskflow (or
/// `[xfer].trusted`) trusts; signatures are still checked normally.
#[derive(Debug)]
struct PeerVerifier {
    trust: Trust,
    algs: WebPkiSupportedAlgorithms,
}

impl PeerVerifier {
    fn check(&self, cert: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        if self.trust.allows(cert.as_ref()) {
            Ok(())
        } else {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }
}

impl ServerCertVerifier for PeerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|()| ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_signature(&self.algs, message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_signature(&self.algs, message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

impl ClientCertVerifier for PeerVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|()| ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_signature(&self.algs, message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_signature(&self.algs, message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// One DER element: (tag, contents, rest).
fn der(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let n = usize::from(first & 0x7f);
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        let len = rest[..n]
            .iter()
            .fold(0usize, |acc, &b| (acc << 8) | usize::from(b));
        (len, &rest[n..])
    };
    (rest.len() >= len).then(|| (tag, &rest[..len], &rest[len..]))
}

/// (AlgorithmIdentifier contents, public key bits) of a certificate's
/// SubjectPublicKeyInfo. Works for X.509 v1 certificates too.
fn spki(cert: &[u8]) -> Option<(&[u8], &[u8])> {
    const SEQUENCE: u8 = 0x30;
    let (SEQUENCE, cert, _) = der(cert)? else {
        return None;
    };
    let (SEQUENCE, tbs, _) = der(cert)? else {
        return None;
    };
    let (tag, _, mut rest) = der(tbs)?;
    if tag == 0xa0 {
        rest = der(rest)?.2; // [0] version present: skip the serial too
    }
    for _ in 0..4 {
        rest = der(rest)?.2; // signature algorithm, issuer, validity, subject
    }
    let (SEQUENCE, spki, _) = der(rest)? else {
        return None;
    };
    let (SEQUENCE, alg, rest) = der(spki)? else {
        return None;
    };
    let (0x03, bits, _) = der(rest)? else {
        return None;
    };
    let (&0, key) = bits.split_first()? else {
        return None;
    };
    Some((alg, key))
}

/// Checks a handshake signature with the certificate's public key.
///
/// Deskflow creates X.509 **v1** certificates, which webpki (rustls' default
/// check) refuses to parse – so the key is taken from the certificate here
/// and the signature verified with the same algorithms rustls would use.
/// Who is trusted is decided by fingerprint, not by the certificate's
/// contents, so nothing else in it matters.
fn verify_signature(
    algs: &WebPkiSupportedAlgorithms,
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
) -> Result<HandshakeSignatureValid, rustls::Error> {
    let (alg_id, key) = spki(cert.as_ref()).ok_or(rustls::Error::InvalidCertificate(
        CertificateError::BadEncoding,
    ))?;
    let candidates = algs
        .mapping
        .iter()
        .find(|(scheme, _)| *scheme == dss.scheme)
        .map(|(_, a)| *a)
        .ok_or_else(|| {
            rustls::Error::General(format!("unsupported signature scheme {:?}", dss.scheme))
        })?;
    let alg = candidates
        .iter()
        .find(|a| a.public_key_alg_id().as_ref() == alg_id)
        .ok_or_else(|| {
            rustls::Error::General("signature scheme does not match the certificate key".into())
        })?;
    alg.verify_signature(key, message, dss.signature())
        .map(|()| HandshakeSignatureValid::assertion())
        .map_err(|_| rustls::Error::InvalidCertificate(CertificateError::BadSignature))
}

/// This machine's certificate + key (Deskflow's `deskflow.pem`).
pub struct Identity {
    pub certs: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
}

impl Identity {
    pub fn load(pem: &Path) -> Result<Self, Error> {
        let bad = |what: &str, e: &dyn std::fmt::Debug| {
            Error::Tls(format!("{}: {what}: {e:?}", pem.display()))
        };
        let certs = CertificateDer::pem_file_iter(pem)
            .map_err(|e| bad("certificate", &e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| bad("certificate", &e))?;
        if certs.is_empty() {
            return Err(Error::Tls(format!("{}: no certificate", pem.display())));
        }
        let key = PrivateKeyDer::from_pem_file(pem).map_err(|e| bad("private key", &e))?;
        Ok(Self { certs, key })
    }

    pub fn fingerprint(&self) -> String {
        trust::fingerprint(self.certs[0].as_ref())
    }
}

/// Client + server TLS configs from an identity and a trust list.
pub fn tls_configs(
    id: &Identity,
    trust: Trust,
) -> Result<(Arc<ClientConfig>, Arc<ServerConfig>), Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(PeerVerifier {
        trust,
        algs: provider.signature_verification_algorithms,
    });
    let tls = |e: rustls::Error| Error::Tls(e.to_string());
    // Built by hand instead of with_single_cert()/with_client_auth_cert():
    // those compare the key with the certificate through webpki, which
    // rejects Deskflow's X.509 v1 certificates.
    let signing_key = provider
        .key_provider
        .load_private_key(id.key.clone_key())
        .map_err(tls)?;
    let certified = Arc::new(CertifiedKey::new(id.certs.clone(), signing_key));
    let client = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(tls)?
        .dangerous()
        .with_custom_certificate_verifier(verifier.clone())
        .with_client_cert_resolver(Arc::new(SingleCertAndKey::from(certified.clone())));
    let server = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(tls)?
        .with_client_cert_verifier(verifier)
        .with_cert_resolver(Arc::new(SingleCertAndKey::from(certified)));
    Ok((Arc::new(client), Arc::new(server)))
}

/// Loads Deskflow's identity + trust as configured.
pub fn load_tls(
    cfg: &XferConfig,
    deskflow_settings: Option<&Path>,
) -> Result<(Identity, Trust), Error> {
    let dir = trust::find_tls_dir(&cfg.tls_dir, deskflow_settings).ok_or(Error::NoTlsDir)?;
    let id = Identity::load(&dir.join(CERT_FILE))?;
    Ok((id, Trust::new(dir, &cfg.trusted)))
}

// ---- service --------------------------------------------------------------------

struct Inner {
    cfg: XferConfig,
    name: String,
    client: Arc<ClientConfig>,
    server: Arc<ServerConfig>,
    /// Learned peer address (`cfg.peer` wins when set).
    peer: Mutex<Option<String>>,
    local: Mutex<VecDeque<Arc<LocalOffer>>>,
    /// Offer currently on this machine's clipboard (to withdraw).
    current: Mutex<Option<u64>>,
    events: Sender<Event>,
    stop: AtomicBool,
    port: u16,
}

/// The transfer service: listens for the other machine and talks to it.
/// Cheap to clone; stops when the last clone is dropped.
#[derive(Clone)]
pub struct Service {
    inner: Arc<Inner>,
    _guard: Arc<StopGuard>,
}

struct StopGuard(Arc<Inner>);

impl Drop for StopGuard {
    fn drop(&mut self) {
        self.0.stop.store(true, Ordering::Relaxed);
        // Wake the blocking accept() so the listener thread ends.
        let _ = TcpStream::connect_timeout(
            &SocketAddr::from((Ipv4Addr::LOCALHOST, self.0.port)),
            Duration::from_millis(200),
        );
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(f) {
        log::warn!("xfer: cannot start thread {name}: {e}");
    }
}

impl Service {
    /// Binds the listener and starts serving. `name` is this machine's
    /// display name (Deskflow screen name).
    pub fn start(
        cfg: &XferConfig,
        name: &str,
        identity: &Identity,
        trust: Trust,
        events: Sender<Event>,
    ) -> Result<Self, Error> {
        let (client, server) = tls_configs(identity, trust)?;
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, cfg.port))?;
        let port = listener.local_addr()?.port();
        let inner = Arc::new(Inner {
            cfg: cfg.clone(),
            name: name.to_string(),
            client,
            server,
            peer: Mutex::new(None),
            local: Mutex::new(VecDeque::new()),
            current: Mutex::new(None),
            events,
            stop: AtomicBool::new(false),
            port,
        });
        log::info!(
            "xfer: listening on port {port} (certificate {})",
            trust::pretty(&identity.fingerprint())
        );
        let li = inner.clone();
        spawn("xfer-listen", move || accept_loop(li, listener));
        let hi = inner.clone();
        spawn("xfer-hello", move || hello_loop(hi));
        Ok(Self {
            _guard: Arc::new(StopGuard(inner.clone())),
            inner,
        })
    }

    pub fn port(&self) -> u16 {
        self.inner.port
    }

    /// The other machine's address (from the config, Deskflow's log, …).
    pub fn set_peer(&self, host: &str) {
        self.inner.learn_peer(host);
    }

    pub fn peer(&self) -> Option<String> {
        self.inner.peer()
    }

    /// Offers local files to the other machine (in the background).
    pub fn offer_local(&self, paths: Vec<PathBuf>) {
        self.client().offer_local(paths);
    }

    /// Downloads and readers, without keeping the service alive.
    pub fn client(&self) -> Client {
        Client {
            inner: self.inner.clone(),
        }
    }
}

/// Talks to the other machine for downloads. Holding one does not keep the
/// [`Service`] (listener) running.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl Client {
    pub fn is_stopped(&self) -> bool {
        self.inner.stop.load(Ordering::Relaxed)
    }

    /// Offers local files to the other machine (in the background).
    pub fn offer_local(&self, paths: Vec<PathBuf>) {
        let inner = self.inner.clone();
        spawn("xfer-offer", move || {
            let offer = match LocalOffer::from_paths(&paths) {
                Ok(o) => Arc::new(o),
                Err(e) => {
                    log::warn!("xfer: cannot offer copied files: {e}");
                    return;
                }
            };
            log::info!(
                "xfer: offering {} item(s), {} bytes total",
                offer.entries.len(),
                offer.total_size()
            );
            {
                let mut local = lock(&inner.local);
                local.push_back(offer.clone());
                while local.len() > KEEP_OFFERS {
                    local.pop_front();
                }
            }
            *lock(&inner.current) = Some(offer.id);
            let req = Request::Offer {
                id: offer.id,
                from: inner.name.clone(),
                entries: offer.entries.clone(),
            };
            match inner.call_peer(&req) {
                Ok(()) => {}
                Err(e) => log::warn!("xfer: cannot offer files to the other machine: {e}"),
            }
        });
    }

    /// The local files are no longer on the clipboard.
    pub fn withdraw_local(&self) {
        let Some(id) = lock(&self.inner.current).take() else {
            return;
        };
        let inner = self.inner.clone();
        spawn("xfer-withdraw", move || {
            if let Err(e) = inner.call_peer(&Request::Withdraw { id }) {
                log::debug!("xfer: withdraw: {e}");
            }
        });
    }

    /// Sequential reader for file `entry` of a remote offer.
    pub fn reader(&self, offer: &RemoteOffer, entry: u32) -> Result<RemoteReader, Error> {
        let e = offer
            .entries
            .get(entry as usize)
            .filter(|e| !e.dir)
            .ok_or_else(|| Error::Remote("no such file in the offer".into()))?;
        Ok(RemoteReader::new(self.inner.clone(), offer, entry, e, 0))
    }

    /// Downloads top-level entry `top` (with everything below it) of a
    /// remote offer into `dest_dir`. Partial files (`*.part`) are resumed.
    /// Returns the path of the entry.
    pub fn download_entry(
        &self,
        offer: &RemoteOffer,
        top: u32,
        dest_dir: &Path,
    ) -> Result<PathBuf, Error> {
        let root = offer
            .entries
            .get(top as usize)
            .ok_or_else(|| Error::Remote("no such entry".into()))?;
        let prefix = format!("{}/", root.path);
        let mut root_path = None;
        for (i, e) in offer.entries.iter().enumerate() {
            if e.path != root.path && !e.path.starts_with(&prefix) {
                continue;
            }
            let parts = safe_components(&e.path)
                .ok_or_else(|| Error::Remote(format!("unsafe path {:?}", e.path)))?;
            let dest = parts.iter().fold(dest_dir.to_path_buf(), |p, c| p.join(c));
            if e.dir {
                fs::create_dir_all(&dest)?;
            } else {
                self.download_file(offer, i as u32, e, &dest)?;
            }
            if root_path.is_none() {
                root_path = Some(dest);
            }
        }
        root_path.ok_or_else(|| Error::Remote("empty entry".into()))
    }

    /// Downloads every entry of `offer` into `dest_dir`.
    pub fn download_all(
        &self,
        offer: &RemoteOffer,
        dest_dir: &Path,
    ) -> Result<Vec<PathBuf>, Error> {
        let tops: Vec<u32> = (0..offer.entries.len() as u32)
            .filter(|&i| offer.entries[i as usize].is_top_level())
            .collect();
        tops.into_iter()
            .map(|i| self.download_entry(offer, i, dest_dir))
            .collect()
    }

    fn download_file(
        &self,
        offer: &RemoteOffer,
        index: u32,
        e: &Entry,
        dest: &Path,
    ) -> Result<(), Error> {
        if fs::metadata(dest).is_ok_and(|m| m.is_file() && m.len() == e.size) {
            return Ok(()); // already downloaded (pasted twice)
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut part_name = dest.file_name().unwrap_or_default().to_os_string();
        part_name.push(".part");
        let part = dest.with_file_name(part_name);
        let mut offset = fs::metadata(&part).map_or(0, |m| m.len());
        if offset > e.size {
            offset = 0;
        }
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&part)?;
        file.set_len(offset)?;
        let mut out = BufWriter::with_capacity(CHUNK, file);
        io::Seek::seek(&mut out, io::SeekFrom::Start(offset))?;
        if offset > 0 {
            log::info!("xfer: resuming {} at {offset} bytes", e.path);
        }
        let mut reader = RemoteReader::new(self.inner.clone(), offer, index, e, offset);
        io::copy(&mut reader, &mut out)
            .map_err(|err| reader.take_error().unwrap_or(Error::Io(err)))?;
        let file = out.into_inner().map_err(|e| Error::Io(e.into_error()))?;
        file.sync_all()?;
        if e.mtime != 0 {
            let _ = file.set_modified(UNIX_EPOCH + Duration::from_secs(e.mtime));
        }
        drop(file);
        fs::rename(&part, dest)?;
        Ok(())
    }

    /// Promises for the clipboard: readers stream on paste (Windows),
    /// `materialize` downloads into `cache_dir/<offer id>/` (macOS).
    pub fn remote_files(&self, offer: &RemoteOffer, cache_dir: PathBuf) -> RemoteFiles {
        let (svc, o) = (self.clone(), offer.clone());
        let open: crate::clip::Opener = Arc::new(move |entry| {
            svc.reader(&o, entry)
                .map(|r| Box::new(r) as Box<dyn Read + Send>)
                .map_err(io::Error::other)
        });
        let (svc, o) = (self.clone(), offer.clone());
        let dir = cache_dir.join(format!("{:016x}", offer.id));
        let materialize: crate::clip::Materializer =
            Arc::new(move |top| svc.download_entry(&o, top, &dir).map_err(io::Error::other));
        RemoteFiles {
            id: offer.id,
            entries: offer.entries.clone(),
            open,
            materialize,
        }
    }
}

impl Inner {
    fn peer(&self) -> Option<String> {
        let configured = self.cfg.peer.trim();
        if !configured.is_empty() {
            return Some(configured.to_string());
        }
        lock(&self.peer).clone()
    }

    fn learn_peer(&self, host: &str) {
        let host = host.trim();
        if host.is_empty() || !self.cfg.peer.trim().is_empty() {
            return;
        }
        let mut p = lock(&self.peer);
        if p.as_deref() != Some(host) {
            log::info!("xfer: other machine at {host}");
            *p = Some(host.to_string());
            drop(p);
            let _ = self.events.send(Event::Peer(host.to_string()));
        }
    }

    fn emit(&self, ev: Event) {
        let _ = self.events.send(ev);
    }

    fn timeout(&self) -> Duration {
        Duration::from_millis(self.cfg.timeout_ms.max(1000))
    }

    fn connect(&self, host: &str) -> Result<ClientStream, Error> {
        let mut last = None;
        for addr in resolve(host, self.cfg.port)? {
            match TcpStream::connect_timeout(&addr, self.timeout()) {
                Ok(tcp) => {
                    tcp.set_read_timeout(Some(self.timeout()))?;
                    tcp.set_write_timeout(Some(self.timeout()))?;
                    tcp.set_nodelay(true)?;
                    let name =
                        ServerName::try_from(SERVER_NAME).map_err(|e| Error::Tls(e.to_string()))?;
                    let conn = ClientConnection::new(self.client.clone(), name)
                        .map_err(|e| Error::Tls(e.to_string()))?;
                    let mut s = StreamOwned::new(conn, tcp);
                    s.write_all(MAGIC)?;
                    return Ok(s);
                }
                Err(e) => last = Some(e),
            }
        }
        Err(last.map_or(Error::NoPeer, Error::Io))
    }

    fn call(&self, host: &str, req: &Request) -> Result<(), Error> {
        let mut s = self.connect(host)?;
        write_msg(&mut s, req)?;
        s.flush()?;
        match read_msg::<Response>(&mut s)? {
            Response::Ok => Ok(()),
            Response::Error { msg } => Err(Error::Remote(msg)),
            Response::Data { .. } => Err(Error::Remote("unexpected data".into())),
        }
    }

    fn call_peer(&self, req: &Request) -> Result<(), Error> {
        let host = self.peer().ok_or(Error::NoPeer)?;
        self.call(&host, req)
    }

    fn find_local(&self, id: u64) -> Option<Arc<LocalOffer>> {
        lock(&self.local).iter().find(|o| o.id == id).cloned()
    }
}

/// `host`, `host:port`, `1.2.3.4:5`, `[fe80::1]:5` or a bare IPv6 address.
fn resolve(host: &str, default_port: u16) -> io::Result<Vec<SocketAddr>> {
    if let Ok(a) = host.parse::<SocketAddr>() {
        return Ok(vec![a]);
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, default_port)]);
    }
    match host.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => {
            let port = p.parse().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, format!("bad port in {host}"))
            })?;
            Ok((h, port).to_socket_addrs()?.collect())
        }
        _ => Ok((host, default_port).to_socket_addrs()?.collect()),
    }
}

fn hello_loop(inner: Arc<Inner>) {
    let mut last: Option<(String, Instant)> = None;
    while !inner.stop.load(Ordering::Relaxed) {
        if let Some(host) = inner.peer() {
            let due = last
                .as_ref()
                .map_or(true, |(h, t)| *h != host || t.elapsed() >= HELLO_EVERY);
            if due {
                let r = inner.call(
                    &host,
                    &Request::Hello {
                        name: inner.name.clone(),
                    },
                );
                if let Err(e) = r {
                    log::debug!("xfer: hello to {host}: {e}");
                }
                last = Some((host, Instant::now()));
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn accept_loop(inner: Arc<Inner>, listener: TcpListener) {
    for conn in listener.incoming() {
        if inner.stop.load(Ordering::Relaxed) {
            break;
        }
        match conn {
            Ok(tcp) => {
                let i = inner.clone();
                spawn("xfer-conn", move || {
                    if let Err(e) = handle_conn(&i, tcp) {
                        log::debug!("xfer: connection: {e}");
                    }
                });
            }
            Err(e) => {
                log::warn!("xfer: accept: {e}");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

fn handle_conn(inner: &Inner, tcp: TcpStream) -> Result<(), Error> {
    let remote: IpAddr = tcp.peer_addr()?.ip();
    tcp.set_read_timeout(Some(inner.timeout()))?;
    tcp.set_write_timeout(Some(inner.timeout()))?;
    tcp.set_nodelay(true)?;
    let conn =
        ServerConnection::new(inner.server.clone()).map_err(|e| Error::Tls(e.to_string()))?;
    let mut s = StreamOwned::new(conn, tcp);
    let mut magic = [0u8; 4];
    s.read_exact(&mut magic)?; // completes the handshake (peer verified) first
    if &magic != MAGIC {
        return Err(Error::Remote("wrong protocol version".into()));
    }
    let req: Request = read_msg(&mut s)?;
    // Only a verified peer gets here – safe to learn its address.
    if !remote.is_loopback() {
        inner.learn_peer(&remote.to_string());
    }
    match req {
        Request::Hello { name } => {
            log::debug!("xfer: hello from {name} ({remote})");
            write_msg(&mut s, &Response::Ok)?;
        }
        Request::Offer { id, from, entries } => {
            if let Err(msg) = validate_entries(&entries) {
                write_msg(&mut s, &Response::Error { msg: msg.clone() })?;
                return Err(Error::Remote(msg));
            }
            write_msg(&mut s, &Response::Ok)?;
            log::info!(
                "xfer: {from} copied {} item(s)",
                entries.iter().filter(|e| e.is_top_level()).count()
            );
            inner.emit(Event::RemoteOffer(RemoteOffer {
                id,
                from,
                entries,
                host: (!remote.is_loopback()).then(|| remote.to_string()),
            }));
        }
        Request::Withdraw { id } => {
            write_msg(&mut s, &Response::Ok)?;
            inner.emit(Event::RemoteWithdrawn { id });
        }
        Request::Get { id, entry, offset } => serve_file(inner, &mut s, id, entry, offset)?,
    }
    s.flush()?;
    s.conn.send_close_notify();
    let _ = s.flush();
    Ok(())
}

fn serve_file(
    inner: &Inner,
    s: &mut StreamOwned<ServerConnection, TcpStream>,
    id: u64,
    entry: u32,
    offset: u64,
) -> Result<(), Error> {
    let opened = inner
        .find_local(id)
        .ok_or_else(|| {
            io::Error::other("the copied files are no longer available – copy them again")
        })
        .and_then(|o| {
            let name = o
                .entries
                .get(entry as usize)
                .map(|e| e.path.clone())
                .unwrap_or_default();
            let total = o.entries.get(entry as usize).map_or(0, |e| e.size);
            o.open(entry, offset).map(|(f, len)| (f, len, name, total))
        });
    let (mut file, len, name, total) = match opened {
        Ok(v) => v,
        Err(e) => {
            write_msg(s, &Response::Error { msg: e.to_string() })?;
            return Err(Error::Io(e));
        }
    };
    write_msg(s, &Response::Data { len })?;
    let mut buf = vec![0u8; CHUNK];
    let mut sent = 0u64;
    let mut last = Instant::now();
    let result = (|| -> io::Result<()> {
        while sent < len {
            let want = (len - sent).min(CHUNK as u64) as usize;
            let n = file.read(&mut buf[..want])?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "file shrank while sending",
                ));
            }
            s.write_all(&buf[..n])?;
            sent += n as u64;
            if last.elapsed() >= PROGRESS_EVERY {
                last = Instant::now();
                inner.emit(Event::Progress(Progress {
                    dir: Direction::Send,
                    id,
                    name: name.clone(),
                    done: offset + sent,
                    total,
                }));
            }
        }
        Ok(())
    })();
    inner.emit(Event::Done {
        dir: Direction::Send,
        id,
        name: name.clone(),
        result: result.as_ref().map(|_| ()).map_err(|e| e.to_string()),
    });
    result.map_err(Error::Io)
}

// ---- resumable reader -------------------------------------------------------------

/// Reads one remote file sequentially. Reconnects after network errors and
/// continues where it stopped (`Get` with the current offset), up to
/// `[xfer].retries` times.
pub struct RemoteReader {
    inner: Arc<Inner>,
    host: String,
    id: u64,
    entry: u32,
    name: String,
    size: u64,
    pos: u64,
    stream: Option<ClientStream>,
    retries_left: u32,
    attempt: u32,
    last_progress: Instant,
    error: Option<Error>,
    finished: bool,
}

impl RemoteReader {
    fn new(inner: Arc<Inner>, offer: &RemoteOffer, entry: u32, e: &Entry, offset: u64) -> Self {
        let host = offer
            .host
            .clone()
            .or_else(|| inner.peer())
            .unwrap_or_default();
        Self {
            retries_left: inner.cfg.retries,
            inner,
            host,
            id: offer.id,
            entry,
            name: e.path.clone(),
            size: e.size,
            pos: offset,
            stream: None,
            attempt: 0,
            last_progress: Instant::now(),
            error: None,
            finished: false,
        }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// The error behind the last failed read, with its original type.
    pub fn take_error(&mut self) -> Option<Error> {
        self.error.take()
    }

    fn open(&self) -> Result<ClientStream, Error> {
        if self.host.is_empty() {
            return Err(Error::NoPeer);
        }
        let mut s = self.inner.connect(&self.host)?;
        write_msg(
            &mut s,
            &Request::Get {
                id: self.id,
                entry: self.entry,
                offset: self.pos,
            },
        )?;
        s.flush()?;
        match read_msg::<Response>(&mut s)? {
            Response::Data { len } if len == self.size - self.pos => Ok(s),
            Response::Data { len } => Err(Error::Remote(format!(
                "size mismatch: expected {} bytes, other machine sends {len}",
                self.size - self.pos
            ))),
            Response::Error { msg } => Err(Error::Remote(msg)),
            Response::Ok => Err(Error::Remote("unexpected answer".into())),
        }
    }

    /// Records a failure; true when another attempt should be made.
    fn retry(&mut self, err: Error) -> bool {
        self.stream = None;
        let retryable = matches!(err, Error::Io(_));
        if retryable && self.retries_left > 0 {
            self.retries_left -= 1;
            self.attempt += 1;
            log::info!(
                "xfer: {}: {err} – reconnecting at {} bytes",
                self.name,
                self.pos
            );
            std::thread::sleep(Duration::from_millis(500 * u64::from(self.attempt.min(6))));
            return true;
        }
        self.finish(Err(err.to_string()));
        self.error = Some(err);
        false
    }

    fn finish(&mut self, result: Result<(), String>) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.inner.emit(Event::Done {
            dir: Direction::Receive,
            id: self.id,
            name: self.name.clone(),
            result,
        });
    }

    fn progress(&mut self) {
        if self.last_progress.elapsed() >= PROGRESS_EVERY || self.pos == self.size {
            self.last_progress = Instant::now();
            self.inner.emit(Event::Progress(Progress {
                dir: Direction::Receive,
                id: self.id,
                name: self.name.clone(),
                done: self.pos,
                total: self.size,
            }));
        }
    }
}

impl Read for RemoteReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.size || buf.is_empty() {
            if self.pos >= self.size {
                self.finish(Ok(()));
            }
            return Ok(0);
        }
        loop {
            if self.stream.is_none() {
                match self.open() {
                    Ok(s) => self.stream = Some(s),
                    Err(e) => {
                        if self.retry(e) {
                            continue;
                        }
                        let msg = self
                            .error
                            .as_ref()
                            .map_or_else(String::new, |e| e.to_string());
                        return Err(io::Error::other(msg));
                    }
                }
            }
            let max = (self.size - self.pos).min(buf.len() as u64) as usize;
            let Some(stream) = self.stream.as_mut() else {
                continue;
            };
            match stream.read(&mut buf[..max]) {
                Ok(0) => {
                    let e = io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed early");
                    if !self.retry(Error::Io(e)) {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "transfer interrupted",
                        ));
                    }
                }
                Ok(n) => {
                    self.pos += n as u64;
                    self.progress();
                    if self.pos == self.size {
                        self.stream = None;
                        self.finish(Ok(()));
                    }
                    return Ok(n);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    let kind = e.kind();
                    if !self.retry(Error::Io(e)) {
                        return Err(io::Error::new(kind, "transfer failed"));
                    }
                }
            }
        }
    }
}

impl Drop for RemoteReader {
    fn drop(&mut self) {
        if !self.finished && self.pos < self.size && self.pos > 0 {
            self.finish(Err("cancelled".into()));
        }
    }
}

/// Removes cache folders older than `max_age` (downloads pasted on macOS).
pub fn clean_cache(dir: &Path, max_age: Duration) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    let now = SystemTime::now();
    for e in rd.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > max_age);
        if old {
            let p = e.path();
            let r = if p.is_dir() {
                fs::remove_dir_all(&p)
            } else {
                fs::remove_file(&p)
            };
            if let Err(err) = r {
                log::debug!("xfer: cache cleanup {}: {err}", p.display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::PrivatePkcs8KeyDer;
    use std::sync::mpsc::{self, Receiver};

    fn identity() -> Identity {
        let ck = rcgen::generate_simple_self_signed(vec!["test".into()]).unwrap();
        Identity {
            certs: vec![ck.cert.der().clone()],
            key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(ck.key_pair.serialize_der())),
        }
    }

    /// Trust folder that trusts `fps` (Deskflow's file format).
    fn trust_for(fps: &[String]) -> (tempfile::TempDir, Trust) {
        let dir = tempfile::tempdir().unwrap();
        let text: String = fps.iter().map(|f| format!("v2:sha256:{f}\n")).collect();
        fs::write(dir.path().join("trusted-clients"), text).unwrap();
        let t = Trust::new(dir.path().into(), &[]);
        (dir, t)
    }

    fn cfg() -> XferConfig {
        XferConfig {
            port: 0,
            retries: 2,
            timeout_ms: 5000,
            ..XferConfig::default()
        }
    }

    struct Pair {
        a: Service,
        b: Service,
        a_rx: Receiver<Event>,
        b_rx: Receiver<Event>,
        _dirs: Vec<tempfile::TempDir>,
    }

    fn pair() -> Pair {
        let (ia, ib) = (identity(), identity());
        let (da, ta) = trust_for(&[ib.fingerprint()]);
        let (db, tb) = trust_for(&[ia.fingerprint()]);
        let (atx, a_rx) = mpsc::channel();
        let (btx, b_rx) = mpsc::channel();
        let a = Service::start(&cfg(), "A", &ia, ta, atx).unwrap();
        let b = Service::start(&cfg(), "B", &ib, tb, btx).unwrap();
        a.set_peer(&format!("127.0.0.1:{}", b.port()));
        b.set_peer(&format!("127.0.0.1:{}", a.port()));
        Pair {
            a,
            b,
            a_rx,
            b_rx,
            _dirs: vec![da, db],
        }
    }

    fn wait_offer(rx: &Receiver<Event>) -> RemoteOffer {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(Event::RemoteOffer(o)) = rx.recv_timeout(Duration::from_millis(100)) {
                return o;
            }
        }
        panic!("no offer received");
    }

    fn data(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
    }

    #[test]
    fn offer_and_download_folder() {
        let p = pair();
        let src = tempfile::tempdir().unwrap();
        let folder = src.path().join("Photos");
        fs::create_dir_all(folder.join("raw")).unwrap();
        let big = data(3 * 1024 * 1024 + 17);
        fs::write(folder.join("raw").join("big.bin"), &big).unwrap();
        fs::write(folder.join("empty.txt"), b"").unwrap();
        fs::write(src.path().join("note.txt"), b"hi").unwrap();

        p.a.offer_local(vec![folder, src.path().join("note.txt")]);
        let offer = wait_offer(&p.b_rx);
        assert_eq!(offer.from, "A");

        let dest = tempfile::tempdir().unwrap();
        let got = p.b.client().download_all(&offer, dest.path()).unwrap();
        assert_eq!(
            got,
            vec![dest.path().join("Photos"), dest.path().join("note.txt")]
        );
        assert_eq!(
            fs::read(dest.path().join("Photos/raw/big.bin")).unwrap(),
            big
        );
        assert_eq!(fs::read(dest.path().join("Photos/empty.txt")).unwrap(), b"");
        assert_eq!(fs::read(dest.path().join("note.txt")).unwrap(), b"hi");
        assert!(!dest.path().join("Photos/raw/big.bin.part").exists());

        // the sender saw progress / completion for the big file
        let sent_done = p.a_rx.try_iter().any(|e| {
            matches!(e, Event::Done { dir: Direction::Send, ref name, result: Ok(()), .. } if name == "Photos/raw/big.bin")
        });
        assert!(sent_done);
    }

    #[test]
    fn resumes_partial_download() {
        let p = pair();
        let src = tempfile::tempdir().unwrap();
        let file = src.path().join("video.bin");
        let content = data(2 * 1024 * 1024);
        fs::write(&file, &content).unwrap();
        p.a.offer_local(vec![file]);
        let offer = wait_offer(&p.b_rx);

        let dest = tempfile::tempdir().unwrap();
        // pretend an earlier attempt got the first 1.5 MB
        fs::write(dest.path().join("video.bin.part"), &content[..1536 * 1024]).unwrap();
        p.b.client().download_all(&offer, dest.path()).unwrap();
        assert_eq!(fs::read(dest.path().join("video.bin")).unwrap(), content);

        // the sender only sent the rest
        let progress: Vec<u64> = p
            .a_rx
            .try_iter()
            .filter_map(|e| match e {
                Event::Progress(pr) if pr.dir == Direction::Send => Some(pr.done),
                _ => None,
            })
            .collect();
        assert!(progress.iter().all(|&d| d > 1536 * 1024), "{progress:?}");
    }

    #[test]
    fn streaming_reader_reads_whole_file() {
        let p = pair();
        let src = tempfile::tempdir().unwrap();
        let file = src.path().join("a.bin");
        let content = data(700_000);
        fs::write(&file, &content).unwrap();
        p.a.offer_local(vec![file]);
        let offer = wait_offer(&p.b_rx);
        let files = p.b.client().remote_files(&offer, src.path().join("cache"));
        let mut r = (files.open)(0).unwrap();
        let mut got = Vec::new();
        r.read_to_end(&mut got).unwrap();
        assert_eq!(got, content);
        // macOS path: download into the cache
        let path = (files.materialize)(0).unwrap();
        assert_eq!(fs::read(path).unwrap(), content);
    }

    #[test]
    fn untrusted_peer_is_rejected() {
        let p = pair();
        let stranger = identity();
        let (_d, t) = trust_for(&[]);
        let (tx, _rx) = mpsc::channel();
        let c = Service::start(&cfg(), "C", &stranger, t, tx).unwrap();
        let r = c.inner.call(
            &format!("127.0.0.1:{}", p.b.port()),
            &Request::Hello { name: "C".into() },
        );
        assert!(r.is_err());
        // and B never learned it as a peer / got an offer
        assert!(p
            .b_rx
            .try_iter()
            .all(|e| !matches!(e, Event::RemoteOffer(_))));
    }

    #[test]
    fn unknown_offer_is_refused() {
        let p = pair();
        let offer = RemoteOffer {
            id: 42,
            from: "A".into(),
            entries: vec![Entry {
                path: "x".into(),
                size: 3,
                dir: false,
                mtime: 0,
            }],
            host: None,
        };
        let dest = tempfile::tempdir().unwrap();
        match p.b.client().download_all(&offer, dest.path()) {
            Err(Error::Remote(msg)) => assert!(msg.contains("no longer available"), "{msg}"),
            other => panic!("{other:?}"),
        }
    }

    /// Handshake with this machine's real Deskflow certificate, if Deskflow is
    /// set up here (skipped otherwise, e.g. in CI).
    #[test]
    fn real_deskflow_certificate_handshakes() {
        let Some(dir) = trust::find_tls_dir("", None) else {
            return;
        };
        let id = Identity::load(&dir.join(CERT_FILE)).unwrap();
        let fp = id.fingerprint();
        let (_d, t) = trust_for(&[fp]);
        let (tx, _rx) = mpsc::channel();
        let s = Service::start(&cfg(), "self", &id, t, tx).unwrap();
        s.inner
            .call(
                &format!("127.0.0.1:{}", s.port()),
                &Request::Hello {
                    name: "self".into(),
                },
            )
            .unwrap();
    }

    #[test]
    fn spki_of_v3_certificate() {
        let ck = rcgen::generate_simple_self_signed(vec!["x".into()]).unwrap();
        let (alg, key) = spki(ck.cert.der()).unwrap();
        // rcgen's default key is ECDSA P-256: id-ecPublicKey + prime256v1, 65-byte point
        assert_eq!(alg, rustls::pki_types::alg_id::ECDSA_P256.as_ref());
        assert_eq!(key.len(), 65);
        assert!(spki(&ck.cert.der()[..40]).is_none());
    }

    #[test]
    fn resolves_hosts_with_and_without_port() {
        assert_eq!(
            resolve("127.0.0.1:9", 1).unwrap(),
            vec![SocketAddr::from(([127, 0, 0, 1], 9))]
        );
        assert_eq!(
            resolve("127.0.0.1", 7).unwrap(),
            vec![SocketAddr::from(([127, 0, 0, 1], 7))]
        );
        assert_eq!(resolve("::1", 7).unwrap()[0].port(), 7);
        assert_eq!(resolve("[::1]:8", 7).unwrap()[0].port(), 8);
        assert!(resolve("localhost:x", 7).is_err());
    }
}
