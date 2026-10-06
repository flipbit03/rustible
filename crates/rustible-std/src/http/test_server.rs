//! A one-thread HTTP/1.1 server on 127.0.0.1 for the module's tests: a
//! fixed table of paths, a hit counter, and a record of every request it
//! was sent (method, path, headers, body). Each response closes the
//! connection, so ureq cannot pool.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// `(path, status, extra headers, body)` the server answers with. A route
/// carrying `X-Omit-Length` answers without a `Content-Length`, so the
/// body's size is unknown until it is read; one carrying `X-Stall-Body`
/// sends its head and then waits that many milliseconds before the body;
/// one carrying `X-Stall-Head` waits that long before sending anything;
/// one carrying `X-Stall-After: <bytes>:<ms>` sends that many bytes of the
/// body, then waits, then sends the rest; one carrying
/// `X-Declare-Length: <bytes>` sends that as its `Content-Length` whatever
/// the body is, so a body can end before what its head declared; and
/// one carrying `X-Echo` answers with the request as it arrived, head and
/// body, as a misbehaving API's error page might.
pub(crate) type Route = (&'static str, u16, Vec<(&'static str, String)>, Vec<u8>);

/// One request as the server read it. Header names are lowercased.
#[derive(Debug, Clone)]
pub(crate) struct Seen {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl Seen {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// What a test holds on to: the base URL, the hit counter, the record.
pub(crate) struct Server {
    pub(crate) base: String,
    pub(crate) hits: Arc<AtomicUsize>,
    pub(crate) seen: Arc<Mutex<Vec<Seen>>>,
}

impl Server {
    pub(crate) fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    pub(crate) fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }
}

pub(crate) fn serve(routes: Vec<Route>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (counter, record) = (hits.clone(), seen.clone());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            let head_end = loop {
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break i + 4;
                }
                match s.read(&mut chunk) {
                    Ok(0) | Err(_) => break buf.len(),
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            };
            let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
            let mut lines = head.lines();
            let mut first = lines.next().unwrap_or_default().split_whitespace();
            let method = first.next().unwrap_or_default().to_string();
            let path = first.next().unwrap_or("/").to_string();
            let headers: Vec<(String, String)> = lines
                .filter_map(|l| l.split_once(':'))
                .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                .collect();
            let len = headers
                .iter()
                .find(|(k, _)| k == "content-length")
                .and_then(|(_, v)| v.parse::<usize>().ok())
                .unwrap_or(0);
            let mut body = buf[head_end..].to_vec();
            while body.len() < len {
                match s.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => body.extend_from_slice(&chunk[..n]),
                }
            }
            counter.fetch_add(1, Ordering::SeqCst);
            record.lock().unwrap().push(Seen {
                method: method.clone(),
                path: path.clone(),
                headers,
                body,
            });
            let raw_request = [
                head.as_bytes(),
                &record.lock().unwrap().last().unwrap().body,
            ]
            .concat();
            let (status, headers, body) = routes
                .iter()
                .find(|(p, ..)| *p == path)
                .map(|(_, st, h, b)| (*st, h.clone(), b.clone()))
                .unwrap_or((404, vec![], b"no such route".to_vec()));
            let reason = match status {
                200 => "OK",
                201 => "Created",
                204 => "No Content",
                301 => "Moved Permanently",
                302 => "Found",
                303 => "See Other",
                307 => "Temporary Redirect",
                308 => "Permanent Redirect",
                404 => "Not Found",
                409 => "Conflict",
                500 => "Internal Server Error",
                _ => "Whatever",
            };
            let omit_length = headers.iter().any(|(k, _)| *k == "X-Omit-Length");
            let ms = |name: &str| {
                headers
                    .iter()
                    .find(|(k, _)| *k == name)
                    .and_then(|(_, v)| v.parse::<u64>().ok())
            };
            let (stall, stall_head) = (ms("X-Stall-Body"), ms("X-Stall-Head"));
            let body = if headers.iter().any(|(k, _)| *k == "X-Echo") {
                raw_request
            } else {
                body
            };
            if let Some(ms) = stall_head {
                std::thread::sleep(Duration::from_millis(ms));
            }
            let length = headers
                .iter()
                .find(|(k, _)| *k == "X-Declare-Length")
                .map_or_else(|| body.len().to_string(), |(_, v)| v.clone());
            let mut out = if omit_length {
                format!("HTTP/1.1 {status} {reason}\r\nConnection: close\r\n")
            } else {
                format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: {length}\r\nConnection: close\r\n"
                )
            };
            for (k, v) in &headers {
                if k.starts_with("X-Omit")
                    || k.starts_with("X-Stall")
                    || k.starts_with("X-Declare")
                    || *k == "X-Echo"
                {
                    continue;
                }
                out.push_str(&format!("{k}: {v}\r\n"));
            }
            out.push_str("\r\n");
            let _ = s.write_all(out.as_bytes());
            let _ = s.flush();
            if let Some(ms) = stall {
                std::thread::sleep(Duration::from_millis(ms));
            }
            let split = headers
                .iter()
                .find(|(k, _)| *k == "X-Stall-After")
                .and_then(|(_, v)| v.split_once(':'))
                .and_then(|(n, ms)| Some((n.parse::<usize>().ok()?, ms.parse::<u64>().ok()?)));
            if method != "HEAD" {
                match split {
                    Some((n, ms)) if n < body.len() => {
                        let _ = s.write_all(&body[..n]);
                        let _ = s.flush();
                        std::thread::sleep(Duration::from_millis(ms));
                        let _ = s.write_all(&body[n..]);
                    }
                    _ => {
                        let _ = s.write_all(&body);
                    }
                }
            }
        }
    });
    Server { base, hits, seen }
}

/// An `https://` server on 127.0.0.1 whose certificate is self-signed, so
/// no client that verifies against Mozilla's roots can finish a handshake
/// with it. Returns its base URL. The certificate and key in `testdata/`
/// were made for this with `openssl req -x509 -newkey ec ... -subj
/// /CN=localhost`, valid for a hundred years, and name nothing real.
pub(crate) fn serve_untrusted_tls() -> String {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let cert =
        CertificateDer::from(include_bytes!("testdata/self-signed-localhost.cert.der").to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        include_bytes!("testdata/self-signed-localhost.key.der").to_vec(),
    ));
    let config = Arc::new(
        rustls::ServerConfig::builder_with_provider(crate::tls::provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("https://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let Ok(mut conn) = rustls::ServerConnection::new(config.clone()) else {
                continue;
            };
            let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
            // The client aborts the handshake; whatever this returns is
            // the end of the connection.
            let _ = conn.complete_io(&mut s);
        }
    });
    base
}
