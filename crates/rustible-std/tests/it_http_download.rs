//! Docker integration test for `http::Download` (vision 8, tier 3).
//! Runs with `RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --test it_http_download`.
//!
//! The container may have no network, so the test binary serves the file
//! itself from a loopback listener started inside the container. That
//! covers the op end to end (request, status handling, redirect, checksum,
//! atomic write, attributes) over plain HTTP; TLS is exercised by the
//! ignored unit test `http::tests::https_download_from_github_with_graviola_tls`.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rustible::prelude::*;
use rustible::sdk::testing::changed_then_ok;
use rustible_std::http::Download;

const HELLO: &[u8] = b"hello from rustible\n";
const HELLO_SHA256: &str = "86a9660ed95754054a62f1dbc68e53ab443dd67c84fa77362a699dbf8604da3d";

/// A one-thread HTTP/1.1 server on 127.0.0.1: `/hello.txt`, a redirect to
/// it at `/redir`, 404 for anything else. Returns its base URL and a hit
/// counter.
fn serve() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                match s.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            let head = String::from_utf8_lossy(&buf).into_owned();
            let path = head
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("/")
                .to_string();
            counter.fetch_add(1, Ordering::SeqCst);
            let (status_line, extra, body): (&str, String, &[u8]) = match path.as_str() {
                "/hello.txt" => ("200 OK", String::new(), HELLO),
                "/redir" => ("302 Found", "Location: /hello.txt\r\n".into(), b""),
                _ => ("404 Not Found", String::new(), b"no such route"),
            };
            let out = format!(
                "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
                body.len()
            );
            let _ = s.write_all(out.as_bytes());
            let _ = s.write_all(body);
        }
    });
    (base, hits)
}

#[rustible::integration_test(images = ["debian:12", "ubuntu:24.04"])]
fn download_changed_then_ok(ctx: &mut Ctx) -> Result<()> {
    let (base, hits) = serve();
    let dir = "/opt/rustible-test";
    let path = "/opt/rustible-test/hello.txt";
    ctx.sys().mkdir_all(dir)?;

    // With a checksum: fetched once, then `ok` without touching the network.
    let (first, second) = changed_then_ok(ctx, "download hello.txt", || {
        Download::get(format!("{base}/hello.txt"))
            .to(path)
            .checksum(format!("sha256:{HELLO_SHA256}"))
            .mode(0o640)
    })?;
    assert!(first.downloaded);
    assert_eq!(first.bytes, HELLO.len() as u64);
    assert_eq!(first.sha256.as_deref(), Some(HELLO_SHA256));
    assert!(!second.downloaded);
    assert_eq!(second.sha256.as_deref(), Some(HELLO_SHA256));
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "the second step made no request"
    );
    assert_eq!(ctx.sys().read(path)?, HELLO);
    let stat = ctx.sys().stat(path)?.expect("file exists");
    assert_eq!(stat.mode, 0o640);

    // Without a checksum an existing file is `ok`; `force` re-downloads.
    let r = ctx.step(
        "download without checksum",
        Download::get(format!("{base}/hello.txt")).to(path),
    )?;
    assert!(!r.changed);
    let r = ctx.step(
        "forced download",
        Download::get(format!("{base}/hello.txt"))
            .to(path)
            .force(true)
            .backup(true),
    )?;
    assert!(r.changed && r.downloaded);
    let backup = r.backup_path.clone().expect("a backup was taken");
    assert_eq!(ctx.sys().read(&backup)?, HELLO);
    assert_eq!(hits.load(Ordering::SeqCst), 2);

    // A redirect is followed.
    let r = ctx.step(
        "download via redirect",
        Download::get(format!("{base}/redir"))
            .to("/opt/rustible-test/redirected.txt")
            .mode(0o755),
    )?;
    assert_eq!(r.sha256.as_deref(), Some(HELLO_SHA256));
    assert_eq!(
        ctx.sys()
            .stat("/opt/rustible-test/redirected.txt")?
            .unwrap()
            .mode,
        0o755
    );

    // A downloaded file keeps its setuid bit when `.owner` is also set.
    // Linux's `chown(2)` clears setuid and setgid on non-directories, so
    // this only holds if `apply_attrs` chowns before it chmods; with the
    // two swapped the file comes out 0o755 and the step reports success.
    let r = ctx.step(
        "download a setuid helper",
        Download::get(format!("{base}/hello.txt"))
            .to("/opt/rustible-test/suid.bin")
            .mode(0o4755)
            .owner(65534, 65534),
    )?;
    assert!(r.changed);
    let suid = ctx.sys().stat("/opt/rustible-test/suid.bin")?.unwrap();
    assert_eq!(
        (suid.mode, suid.uid, suid.gid),
        (0o4755, 65534, 65534),
        "chown must not have cleared the setuid bit"
    );

    // A body over `.max_bytes` is refused, naming the limit and the flag,
    // and nothing is written.
    let err = ctx
        .step(
            "download over the size limit",
            Download::get(format!("{base}/hello.txt"))
                .to("/opt/rustible-test/toobig.txt")
                .max_bytes(4),
        )
        .unwrap_err()
        .chain();
    assert!(err.contains("4 byte limit"), "{err}");
    assert!(err.contains(".max_bytes()"), "{err}");
    assert!(!ctx.sys().exists("/opt/rustible-test/toobig.txt")?);

    // Failures name the status and the URL, and write nothing.
    let url = format!("{base}/missing.txt");
    let err = ctx
        .step(
            "download a 404",
            Download::get(&url).to("/opt/rustible-test/missing.txt"),
        )
        .unwrap_err()
        .chain();
    assert!(
        err.contains(&format!("GET {url} returned 404 Not Found")),
        "{err}"
    );
    assert!(!ctx.sys().exists("/opt/rustible-test/missing.txt")?);

    let want = "0".repeat(64);
    let err = ctx
        .step(
            "download with a wrong checksum",
            Download::get(format!("{base}/hello.txt"))
                .to("/opt/rustible-test/wrong.txt")
                .checksum(format!("sha256:{want}")),
        )
        .unwrap_err()
        .chain();
    assert!(err.contains("sha256 checksum mismatch"), "{err}");
    assert!(
        err.contains(&format!("got {HELLO_SHA256}, want {want}")),
        "{err}"
    );
    assert!(!ctx.sys().exists("/opt/rustible-test/wrong.txt")?);

    // The parent directory is not created on the way (vision 6.7).
    let err = ctx
        .step(
            "download into a missing directory",
            Download::get(format!("{base}/hello.txt")).to("/opt/nope/hello.txt"),
        )
        .unwrap_err()
        .chain();
    assert!(
        err.contains("/opt/nope does not exist; create it first with file::Directory"),
        "{err}"
    );

    ctx.sys().remove_all(dir)?;
    Ok(())
}
