//! Noyra Sidecar — minimal mTLS proxy.
//!
//! Listens on TLS_PORT, terminates mTLS, forwards plaintext to BACKEND_PORT
//! on localhost. Plain HTTP health on HEALTH_PORT. SIGHUP reloads certs.
//!
//! Env vars (all optional):
//!   TLS_PORT     port the sidecar listens on    (default: 8443)
//!   BACKEND_PORT port of the local app          (default: 8080)
//!   HEALTH_PORT  port for the health endpoint   (default: 9090)
//!   CERT_FILE    PEM certificate                (default: /certs/tls.crt)
//!   KEY_FILE     PEM private key                (default: /certs/tls.key)
//!   CA_FILE      CA cert for mTLS verification  (default: /certs/ca.crt)

use std::{fs::File, io::BufReader, net::Ipv4Addr, sync::Arc};

use rustls::{server::WebPkiClientVerifier, RootCertStore, ServerConfig};
use tokio::{
    io::{self, AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    signal::unix::{signal, SignalKind},
    sync::watch,
};
use tokio_rustls::TlsAcceptor;

// Tunables — picked for the smallest possible memory footprint.
const TLS_MAX_FRAGMENT: usize = 4096; // rustls plaintext cap (~10 KiB/session vs ~32 KiB)
const COPY_BUF: usize = 4096; // per-direction proxy buffer (vs default 8 KiB)
const PEM_BUF: usize = 1024; // tiny BufReader for one-shot PEM reads
const HEALTH_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nOK\n";

// Cert paths — the only state outliving startup (for SIGHUP reload).
struct Paths {
    cert: Box<str>,
    key: Box<str>,
    ca: Box<str>,
}

fn env_u16(key: &str, default: u16) -> u16 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn env_str(key: &str, default: &str) -> Box<str> {
    std::env::var(key).unwrap_or_else(|_| default.into()).into_boxed_str()
}

// Returns Option to keep error-handling code small — Box<dyn Error> would
// drag in vtables + Display impls for every leaf error type.
fn load_tls_config(paths: &Paths) -> Option<Arc<ServerConfig>> {
    let pem = |p: &str| File::open(p).ok().map(|f| BufReader::with_capacity(PEM_BUF, f));

    let mut roots = RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut pem(&paths.ca)?) {
        roots.add(cert.ok()?).ok()?;
    }
    let certs = rustls_pemfile::certs(&mut pem(&paths.cert)?)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let key = rustls_pemfile::private_key(&mut pem(&paths.key)?).ok()??;
    let verifier = WebPkiClientVerifier::builder(Arc::new(roots)).build().ok()?;

    let mut cfg = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .ok()?;
    cfg.max_fragment_size = Some(TLS_MAX_FRAGMENT);
    Some(Arc::new(cfg))
}

// Half-duplex copy with a small fixed stack buffer.
async fn copy_one_way<R, W>(mut r: R, mut w: W) -> io::Result<()>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let mut buf = [0u8; COPY_BUF];
    loop {
        match r.read(&mut buf).await? {
            0 => return w.shutdown().await,
            n => w.write_all(&buf[..n]).await?,
        }
    }
}

async fn proxy(tls: tokio_rustls::server::TlsStream<TcpStream>, backend: std::net::SocketAddr) {
    let be = match TcpStream::connect(backend).await {
        Ok(s) => s,
        Err(e) => return eprintln!("[proxy] backend connect failed: {e}"),
    };
    let _ = be.set_nodelay(true);

    let (tr, tw) = io::split(tls);
    let (br, bw) = io::split(be);
    tokio::select! {
        _ = copy_one_way(tr, bw) => {}
        _ = copy_one_way(br, tw) => {}
    }
}

// Health endpoint — single-task accept loop, no per-request spawn.
async fn health_server(addr: std::net::SocketAddr) {
    let listener = TcpListener::bind(addr).await.expect("health bind failed");
    println!("[health] listening on {addr}");
    loop {
        let Ok((mut s, _)) = listener.accept().await else { continue };
        let _ = s.write_all(HEALTH_RESPONSE).await;
        let _ = s.shutdown().await;
    }
}

fn main() {
    // RLIMIT_STACK shrinks future-spawned threads (signal driver, blocking pool).
    // Doesn't affect the main thread (kernel-allocated before we run).
    #[cfg(unix)]
    unsafe {
        let lim = libc::rlimit { rlim_cur: 64 << 10, rlim_max: 64 << 10 };
        let _ = libc::setrlimit(libc::RLIMIT_STACK, &lim);
    }

    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .max_blocking_threads(1)
        .thread_stack_size(64 << 10)
        .build()
        .expect("tokio runtime build failed")
        .block_on(async_main());
}

async fn async_main() {
    let tls_addr: std::net::SocketAddr = (Ipv4Addr::UNSPECIFIED, env_u16("TLS_PORT", 8443)).into();
    let backend_addr: std::net::SocketAddr =
        (Ipv4Addr::LOCALHOST, env_u16("BACKEND_PORT", 8080)).into();
    let health_addr: std::net::SocketAddr =
        (Ipv4Addr::UNSPECIFIED, env_u16("HEALTH_PORT", 9090)).into();
    let paths = Arc::new(Paths {
        cert: env_str("CERT_FILE", "/certs/tls.crt"),
        key: env_str("KEY_FILE", "/certs/tls.key"),
        ca: env_str("CA_FILE", "/certs/ca.crt"),
    });

    let initial = load_tls_config(&paths).expect("failed to load initial TLS config");
    // Watch holds Arc<TlsAcceptor> directly — built once per cert reload.
    let (tx, rx) = watch::channel(Arc::new(TlsAcceptor::from(initial)));

    // SIGHUP -> hot-reload certs (non-fatal on failure).
    {
        let paths = Arc::clone(&paths);
        tokio::spawn(async move {
            let mut sighup = signal(SignalKind::hangup()).expect("SIGHUP handler failed");
            loop {
                sighup.recv().await;
                println!("[tls] SIGHUP received — reloading certs");
                match load_tls_config(&paths) {
                    Some(cfg) => {
                        let _ = tx.send(Arc::new(TlsAcceptor::from(cfg)));
                        println!("[tls] certs reloaded");
                    }
                    None => eprintln!("[tls] reload failed (check cert paths/format)"),
                }
            }
        });
    }

    tokio::spawn(health_server(health_addr));

    let listener = TcpListener::bind(tls_addr).await.expect("TLS bind failed");
    println!("[proxy] mTLS listening on {tls_addr}");
    println!("[proxy] forwarding to {backend_addr}");

    // Init done. Tell the kernel to evict our currently-resident hot pages —
    // cold init code (rustls cert load, etc.) will re-fault from disk only if
    // ever needed. Drops idle RSS by ~120 KB on Linux musl scratch.
    madvise_cold_text();

    loop {
        let Ok((stream, peer)) = listener.accept().await else { continue };
        let acceptor = rx.borrow().clone();
        tokio::spawn(async move {
            match acceptor.accept(stream).await {
                Ok(tls) => proxy(tls, backend_addr).await,
                Err(e) => eprintln!("[tls] handshake failed from {peer}: {e}"),
            }
        });
    }
}

/// Advise the kernel that all currently-resident r-x pages of our binary can
/// be evicted. SAFETY: file-backed exec mappings re-fault from disk on access
/// (~10µs once, negligible). Must be called AFTER all init touches finish.
fn madvise_cold_text() {
    #[cfg(target_os = "linux")]
    {
        let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else { return };
        for line in maps.lines() {
            if !line.contains("/noyra-sidecar") || !line.contains("r-xp") {
                continue;
            }
            let Some((s, e)) = line.split_whitespace().next().and_then(|r| r.split_once('-'))
            else {
                continue;
            };
            let (Ok(start), Ok(end)) = (usize::from_str_radix(s, 16), usize::from_str_radix(e, 16))
            else {
                continue;
            };
            unsafe {
                libc::madvise(start as *mut _, end - start, libc::MADV_DONTNEED);
            }
        }
    }
}
