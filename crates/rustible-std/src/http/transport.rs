//! What every connection the client opens goes through: ureq's own
//! transport (TCP, wrapped in TLS for `https://`), wrapped once more in
//! [`Checked`], which [`CheckedConnector`] puts at the end of ureq's
//! default connector chain. It adds two things ureq does not do.
//!
//! **A body ends only at a clean close.** ureq reads a body framed by
//! neither a `Content-Length` nor chunked encoding until the server closes
//! the connection, and treats a reset, an abort and an unexpected EOF as
//! that close (`run.rs`, lines 676-688 in ureq 3.4.1), so such a body cut
//! short is taken as whole. [`Checked`] turns those three errors into one
//! of kind `Other`, which ureq reports, so only a clean FIN, or over TLS a
//! `close_notify`, ends such a body: a TLS stream that ends without one is
//! `UnexpectedEof` from rustls. A body with a length or chunked framing is
//! not affected, since ureq stops reading it at its end without asking the
//! transport again.
//!
//! **No wait for the body lasts longer than the op's timeout.** ureq's
//! timeouts are deadlines for a phase, and [`Download`](super::Download)
//! gives the body none, so a server that stops sending mid-body would hang
//! the step. While a body is read, [`idle_cap`] names a longest wait, and
//! each `await_input` passes ureq the shorter of it and ureq's own timeout:
//! a slow body that keeps arriving is never cut, a silent one fails. It is
//! carried by a thread-local because a connection outlives the request
//! that opened it (ureq pools them), so the transport cannot be told at
//! connect time; the body is read on the caller's thread, which is where
//! ureq calls `await_input`.
//!
//! `ureq::unversioned::transport` does not follow semver: ureq may change
//! it in a minor release, and a ureq bump must re-run these tests.

use std::cell::Cell;
use std::io;
use std::time::Duration;

use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, NextTimeout, Transport, time,
};

/// Wraps whatever transport the connectors before it produced in
/// [`Checked`]. Chained after ureq's `DefaultConnector`.
#[derive(Debug)]
pub(crate) struct CheckedConnector;

impl<In: Transport> Connector<In> for CheckedConnector {
    type Out = Checked<In>;

    fn connect(
        &self,
        _: &ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        Ok(chained.map(|inner| Checked { inner }))
    }
}

/// A transport whose reads report an unclean close as an error and wait no
/// longer than [`idle_cap`]; see the module docs.
#[derive(Debug)]
pub(crate) struct Checked<T> {
    inner: T,
}

impl<T: Transport> Transport for Checked<T> {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.inner.buffers()
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.inner.transmit_output(amount, timeout)
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        let timeout = capped(timeout, IDLE.with(Cell::get));
        self.inner.await_input(timeout).map_err(unclean_close)
    }

    fn is_open(&mut self) -> bool {
        self.inner.is_open()
    }

    fn is_tls(&self) -> bool {
        self.inner.is_tls()
    }
}

thread_local! {
    /// The longest one wait for input may last on this thread, while a
    /// body is being read under [`idle_cap`].
    static IDLE: Cell<Option<Duration>> = const { Cell::new(None) };
}

/// Run `f` (a read of a body) with every wait for input on this thread
/// capped at `cap`; `None` leaves ureq's own timeouts alone. A wait the cap
/// cuts short fails as `ureq::Error::Timeout(Timeout::RecvBody)`.
pub(crate) fn idle_cap<R>(cap: Option<Duration>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Duration>);
    impl Drop for Restore {
        fn drop(&mut self) {
            IDLE.with(|c| c.set(self.0));
        }
    }
    let _restore = Restore(IDLE.with(|c| c.replace(cap)));
    f()
}

/// The wait ureq asked for, or `cap` when that is shorter. Pure.
fn capped(timeout: NextTimeout, cap: Option<Duration>) -> NextTimeout {
    match cap {
        Some(cap) if cap < *timeout.after => NextTimeout {
            after: time::Duration::Exact(cap),
            reason: ureq::Timeout::RecvBody,
        },
        _ => timeout,
    }
}

/// A reset, an abort or an unexpected EOF as an error ureq reports rather
/// than takes for the end of a body. Anything else as it was.
fn unclean_close(e: ureq::Error) -> ureq::Error {
    let ureq::Error::Io(io) = e else {
        return e;
    };
    let what = match io.kind() {
        io::ErrorKind::ConnectionReset => "the server reset the connection",
        io::ErrorKind::ConnectionAborted => "the connection was aborted",
        io::ErrorKind::UnexpectedEof => "the connection ended without a TLS close_notify",
        _ => return ureq::Error::Io(io),
    };
    ureq::Error::Io(io::Error::other(format!(
        "{what} before the response was complete ({io})"
    )))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use ureq::unversioned::transport::LazyBuffers;

    use super::*;

    /// A transport that answers each `await_input` with the next scripted
    /// result, appending the bytes of an `Ok`, and records the timeout it
    /// was given.
    #[derive(Debug)]
    struct Script {
        buffers: LazyBuffers,
        steps: VecDeque<Result<Vec<u8>, io::ErrorKind>>,
        seen: Vec<NextTimeout>,
    }

    impl Script {
        fn new(steps: Vec<Result<Vec<u8>, io::ErrorKind>>) -> Checked<Script> {
            Checked {
                inner: Script {
                    buffers: LazyBuffers::new(1024, 1024),
                    steps: steps.into(),
                    seen: vec![],
                },
            }
        }
    }

    impl Transport for Script {
        fn buffers(&mut self) -> &mut dyn Buffers {
            &mut self.buffers
        }

        fn transmit_output(&mut self, _: usize, _: NextTimeout) -> Result<(), ureq::Error> {
            Ok(())
        }

        fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
            self.seen.push(timeout);
            match self.steps.pop_front().expect("a scripted step") {
                Ok(bytes) => {
                    let buf = self.buffers.input_append_buf();
                    buf[..bytes.len()].copy_from_slice(&bytes);
                    self.buffers.input_appended(bytes.len());
                    Ok(!bytes.is_empty())
                }
                Err(kind) => Err(ureq::Error::Io(io::Error::new(kind, "scripted"))),
            }
        }

        fn is_open(&mut self) -> bool {
            true
        }
    }

    fn forever() -> NextTimeout {
        NextTimeout {
            after: time::Duration::NotHappening,
            reason: ureq::Timeout::Global,
        }
    }

    /// The three errors ureq takes for the end of a close-delimited body
    /// (run.rs 676-688 in 3.4.1), which is how a TLS stream that ends
    /// without `close_notify` reaches it (rustls says `UnexpectedEof`), are
    /// errors of another kind after the wrapper, each saying what happened.
    #[test]
    fn an_unclean_close_is_an_error_ureq_reports() {
        for (kind, words) in [
            (io::ErrorKind::UnexpectedEof, "without a TLS close_notify"),
            (io::ErrorKind::ConnectionReset, "reset the connection"),
            (io::ErrorKind::ConnectionAborted, "was aborted"),
        ] {
            let mut t = Script::new(vec![Ok(b"partial".to_vec()), Err(kind)]);
            assert!(t.await_input(forever()).unwrap());
            let e = t.await_input(forever()).unwrap_err();
            let ureq::Error::Io(io) = &e else {
                panic!("{e:?}")
            };
            assert_eq!(io.kind(), io::ErrorKind::Other, "{kind:?}");
            let text = e.to_string();
            assert!(text.contains(words), "{text}");
            assert!(text.contains("before the response was complete"), "{text}");
        }
    }

    /// A clean close, the end ureq already knows, passes through untouched,
    /// and so does any other error.
    #[test]
    fn a_clean_close_and_other_errors_pass_through() {
        let mut t = Script::new(vec![Ok(vec![]), Err(io::ErrorKind::BrokenPipe)]);
        assert!(!t.await_input(forever()).unwrap());
        let e = t.await_input(forever()).unwrap_err();
        let ureq::Error::Io(io) = &e else {
            panic!("{e:?}")
        };
        assert_eq!(io.kind(), io::ErrorKind::BrokenPipe);
    }

    /// Under `idle_cap`, each wait is the shorter of the cap and ureq's own
    /// timeout, and the cap is gone once the read returns.
    #[test]
    fn the_cap_shortens_each_wait_and_only_inside_the_read() {
        let mut t = Script::new(vec![Ok(b"a".to_vec()); 4]);
        let cap = Duration::from_millis(300);
        let soon = NextTimeout {
            after: time::Duration::Exact(Duration::from_millis(100)),
            reason: ureq::Timeout::Global,
        };
        idle_cap(Some(cap), || {
            t.await_input(forever()).unwrap();
            t.await_input(soon).unwrap();
        });
        t.await_input(forever()).unwrap();
        idle_cap(None, || t.await_input(forever()).unwrap());
        assert_eq!(
            t.inner.seen,
            [
                NextTimeout {
                    after: time::Duration::Exact(cap),
                    reason: ureq::Timeout::RecvBody,
                },
                soon,
                forever(),
                forever(),
            ]
        );
    }
}
