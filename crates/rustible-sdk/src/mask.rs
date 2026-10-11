//! The run's secret masker. Every secret the run knows is registered here,
//! and every text field of every event the playbook binary emits is masked
//! before it leaves the binary: before the frame goes over SSH, before the
//! local printer, before `--json`.
//!
//! An op does nothing for a secret that arrives as a
//! [`Secret`](crate::Secret): `Secret::new`, its `Deserialize` (the
//! escalation password) and `ctx.local_secret` register it. [`register`] is
//! the one call for a secret that arrives any other way (the password in a
//! URL's `user:pass@`) or that an op derives itself (`Basic
//! base64(user:pass)`, a token put in a query string). An op never masks
//! anything itself, with one exception: an op that cuts or reshapes text
//! that may hold a secret before emitting it calls [`apply_prefix`] first.
//!
//! The rules are Ansible's `SecretMasker`
//! (`lib/ansible/module_utils/_internal/_secrets.py`):
//!
//! - Surrounding spaces, tabs, carriage returns and newlines are not part of
//!   a secret, and are stripped before it is registered.
//! - A secret shorter than **4** characters is never registered, so never
//!   masked: masking it would turn every occurrence of a common short string
//!   in the output into [`PLACEHOLDER`].
//! - A secret of **4 to 6** characters is masked only as a whole word: both
//!   neighbours are not alphanumeric, or are the end of the text. A
//!   password `test` leaves `testing` alone. An occurrence that overlaps or
//!   touches another secret's is masked regardless.
//! - Overlapping and touching occurrences become one placeholder, so no
//!   character of a secret stays visible and the number of secrets is not
//!   revealed.
//! - A secret longer than 65536 characters is registered as its first
//!   65536.
//!
//! Lengths count characters, not bytes. Besides the value itself, each
//! secret is registered in the forms that carry it in the run's output:
//! JSON-escaped, both as Rustible writes JSON (UTF-8) and as an
//! ASCII-only encoder writes it (`\u00fc`); and form-encoded as
//! `http::Request::form` writes it (`p@ss` as `p%40ss`).
//!
//! The runtime also installs a panic hook that masks a panic's message the
//! same way before it is printed to stderr, which `rustible` shows.
//!
//! What is registered is kept in buffers that are wiped when they are
//! dropped, and when the run ends; it is never written anywhere. Masking has
//! no error to report: it cannot fail, and it never panics with a value in
//! the message.

use std::any::Any;
use std::panic::{AssertUnwindSafe, PanicHookInfo, catch_unwind};
use std::sync::{Arc, Once, PoisonError, RwLock, TryLockError};

use zeroize::Zeroizing;

use crate::event::{Event, EventSink, SharedSink};

/// What a masked secret reads as.
pub const PLACEHOLDER: &str = "<secret>";

/// Below this many characters a secret is not registered at all.
const MIN_CHARS: usize = 4;
/// Up to this many characters a secret is masked only as a whole word.
const MAX_SHORT_CHARS: usize = 6;
/// A longer secret is registered as its first this many characters.
const MAX_CHARS: usize = 65536;
/// Stripped from both ends before a secret is registered.
const STRIP: [char; 4] = [' ', '\t', '\r', '\n'];
/// How far past `max` plus the longest form [`apply_prefix`] looks: a short
/// occurrence on the cut (at most [`MAX_SHORT_CHARS`] characters of 4
/// bytes) is masked when another touches it, which can reach the longest
/// form past its end, and is a whole word or not by the character after it.
const LOOKAHEAD: usize = 4 * (MAX_SHORT_CHARS + 1);

/// Register `secret` with the run's masker, so that from now on every event
/// the binary emits shows [`PLACEHOLDER`] wherever it appears, in any of
/// the forms the [module docs](self) list.
///
/// For a secret that is not a [`Secret`](crate::Secret), which registers
/// itself: a value received in another form (the password of a URL's
/// `user:pass@`), or one the op derives (the `base64(user:pass)` of a Basic
/// `Authorization` header, a token put in a query string). Call it as soon
/// as the value exists, before anything could print it. Registering a value
/// twice is harmless; one shorter than 4 characters is ignored.
pub fn register(secret: &str) {
    registry().register(secret);
}

/// The start of a text, masked, for an op that cuts or reshapes text that
/// may hold a secret before emitting it. **Call this first**, and cut or
/// reshape only what it returns: the run masks every event as it leaves the
/// binary, but by then a secret cut in two, or one whose whitespace or
/// control characters were rewritten, no longer matches, and its visible
/// part goes out as it is. Anything that is emitted as it came needs
/// nothing.
///
/// `text` is called once, with the number of bytes to return: the caller
/// returns at least that many bytes of the text, or all of it when it is
/// shorter; more is fine, and is not looked at. Never cut it to `max`
/// yourself: that is the cut this call exists to make safe. That number is
/// `max` and far enough past it to see whole a secret on the cut and one
/// touching it, and a byte more to tell whether the text goes on, so a
/// caller reading from a stream or building the text reads or builds no
/// more than that. It returns the masked start: no more than
/// `max` bytes of the text, cut on a character boundary, and at most `max`
/// bytes long, except that a secret starting before the cut is kept whole
/// as [`PLACEHOLDER`], which can run past `max` by less than its own
/// length. The flag says whether that start is not all of the text, for
/// the caller to say it was cut.
///
/// ```
/// use rustible_sdk::mask;
///
/// let page = "token=0123-abcd-4567 rejected; see the log for the details";
/// mask::register("0123-abcd-4567");
/// let (start, cut) = mask::apply_prefix(20, |_| page);
/// assert_eq!(start, "token=<secret>");
/// assert!(cut);
/// ```
///
/// It masks with the run's registry, as the run does where events leave,
/// so the result is what the run would have emitted for that start.
pub fn apply_prefix<T: AsRef<str>>(max: usize, text: impl FnOnce(usize) -> T) -> (String, bool) {
    let registry = registry();
    let want = max
        .saturating_add(registry.longest())
        .saturating_add(LOOKAHEAD);
    // A byte past the window, so a masked run that ends exactly at its end
    // is still known to be followed by more text.
    let text = text(want.saturating_add(1));
    registry.prefix(text.as_ref(), want, max)
}

/// Forget every registered secret, wiping each. The runtime calls it once
/// the run's last event is emitted: the registry is a static, which is never
/// dropped, so this is what wipes it before the process exits.
pub(crate) fn wipe() {
    let registry = registry();
    let mut reg = registry.0.write().unwrap_or_else(PoisonError::into_inner);
    reg.secrets.clear();
    reg.forms.clear();
}

/// [`register`] for a secret's bytes. Not UTF-8, they are registered as
/// their lossy decoding, which is how a command's stderr is decoded into
/// [`CmdFailed`](crate::error::CmdFailed).
pub(crate) fn register_bytes(bytes: &[u8]) {
    match std::str::from_utf8(bytes) {
        Ok(text) => register(text),
        Err(_) => {
            let lossy = Zeroizing::new(String::from_utf8_lossy(bytes).into_owned());
            register(&lossy);
        }
    }
}

/// The run's registry, which [`register`] adds to and a [`Masking`] sink
/// masks from.
///
/// One per process: a playbook binary is exactly one run, and every thread
/// of it shares the run's secrets.
#[cfg(not(any(test, feature = "testing")))]
fn registry() -> Arc<Registry> {
    static GLOBAL: std::sync::OnceLock<Arc<Registry>> = std::sync::OnceLock::new();
    GLOBAL.get_or_init(Arc::default).clone()
}

/// Under the `testing` feature, one registry per thread instead of per
/// process: tests run in parallel in one process, and one test's secret
/// must not mask another test's output. A test registers and emits on its
/// own thread, so it sees exactly its own secrets; a secret registered on a
/// thread the test spawned is not masked by a sink the test built.
#[cfg(any(test, feature = "testing"))]
fn registry() -> Arc<Registry> {
    thread_local! {
        static SCOPED: Arc<Registry> = Arc::default();
    }
    // An empty one once the thread's own is gone: a panic hook can run
    // while a thread is being torn down.
    SCOPED.try_with(Arc::clone).unwrap_or_default()
}

/// The secrets registered so far, and every form each is matched in.
#[derive(Default)]
pub(crate) struct Registry(RwLock<Forms>);

#[derive(Default)]
struct Forms {
    /// The secrets as registered: stripped and capped. Kept apart from
    /// `forms` because a secret can be another's escaped form, and is still
    /// a secret of its own whose own forms must be added.
    secrets: Vec<Zeroizing<String>>,
    /// Every string the matcher looks for: each secret and its encodings,
    /// each once.
    forms: Vec<Zeroizing<String>>,
}

impl Registry {
    fn register(&self, secret: &str) {
        let trimmed = secret.trim_matches(STRIP);
        let capped = match trimmed.char_indices().nth(MAX_CHARS) {
            Some((at, _)) => trimmed.get(..at).unwrap_or(trimmed),
            None => trimmed,
        };
        if capped.chars().count() < MIN_CHARS {
            return;
        }
        let mut guard = self.0.write().unwrap_or_else(PoisonError::into_inner);
        let reg = &mut *guard;
        if reg.secrets.iter().any(|s| s.as_str() == capped) {
            return;
        }
        reg.secrets.push(Zeroizing::new(capped.to_owned()));
        for form in [
            Zeroizing::new(capped.to_owned()),
            json_escaped(capped, false),
            json_escaped(capped, true),
            form_encoded(capped),
        ] {
            if !reg.forms.contains(&form) {
                reg.forms.push(form);
            }
        }
    }

    /// Whether nothing is registered, so nothing can need masking.
    fn is_empty(&self) -> bool {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .forms
            .is_empty()
    }

    /// `text` with every qualifying occurrence of a registered secret
    /// replaced by [`PLACEHOLDER`]. Returned as it came when nothing in it
    /// is a secret, without a copy.
    pub(crate) fn mask(&self, text: String) -> String {
        let spans = {
            let reg = self.0.read().unwrap_or_else(PoisonError::into_inner);
            merge(&text, occurrences(&text, &reg.forms))
        };
        if spans.is_empty() {
            return text;
        }
        let out = replaced(&text, &spans);
        // The original may hold a secret: wipe it rather than free it as is.
        drop(Zeroizing::new(text));
        out
    }

    /// The length in bytes of the longest form registered, 0 for none.
    fn longest(&self) -> usize {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .forms
            .iter()
            .map(|f| f.len())
            .max()
            .unwrap_or(0)
    }

    /// [`apply_prefix`] once it has the text: mask the first `want` bytes,
    /// and keep at most `max` bytes of the text, as at most `max` bytes and
    /// a placeholder that starts before them.
    ///
    /// Every occurrence that starts before `max` ends within `want`, and a
    /// short one is seen with the character after it and with an occurrence
    /// touching it, which decide whether it is masked. Text past `max` is
    /// never shown, so an occurrence that the end of the window cuts in two
    /// is never shown either.
    fn prefix(&self, text: &str, want: usize, max: usize) -> (String, bool) {
        let window = text
            .get(..text.floor_char_boundary(want))
            .unwrap_or_default();
        let spans = {
            let reg = self.0.read().unwrap_or_else(PoisonError::into_inner);
            merge(window, occurrences(window, &reg.forms))
        };
        let mut spans = spans.into_iter();
        let mut out = String::with_capacity(max.min(window.len()));
        // How much of `window` is shown so far.
        let mut at = 0;
        loop {
            let span = spans.next();
            let plain_end = span.map_or(window.len(), |(start, _)| start);
            // The text up to the next span, as far as both limits allow.
            let stop = plain_end
                .min(max.max(at))
                .min(at.saturating_add(max.saturating_sub(out.len())));
            let stop = window.floor_char_boundary(stop);
            out.push_str(window.get(at..stop).unwrap_or_default());
            if stop < plain_end {
                return (out, true);
            }
            let Some((start, end)) = span else {
                return (out, window.len() < text.len());
            };
            if start >= max || out.len() >= max {
                return (out, true);
            }
            out.push_str(PLACEHOLDER);
            at = end;
        }
    }

    /// [`Registry::mask`] without waiting for the lock, for the panic hook:
    /// `None` when the lock is held for writing (a registration on another
    /// thread, or the very one that panicked), rather than block or
    /// deadlock in the hook.
    fn try_mask(&self, text: &str) -> Option<String> {
        let reg = match self.0.try_read() {
            Ok(reg) => reg,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        let spans = merge(text, occurrences(text, &reg.forms));
        Some(replaced(text, &spans))
    }

    /// `event` with every string in it masked.
    fn mask_event(&self, event: Event) -> Event {
        if self.is_empty() {
            return event;
        }
        event.map_text(&mut |s| self.mask(s))
    }
}

/// `text` with each of `spans` replaced by [`PLACEHOLDER`].
fn replaced(text: &str, spans: &[(usize, usize)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for &(start, end) in spans {
        out.push_str(text.get(at..start).unwrap_or_default());
        out.push_str(PLACEHOLDER);
        at = end;
    }
    out.push_str(text.get(at..).unwrap_or_default());
    out
}

/// Every occurrence of every form in `text`, overlapping ones included, as
/// byte ranges ordered by start and, at one start, longest first.
fn occurrences(text: &str, forms: &[Zeroizing<String>]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    for form in forms {
        let form = form.as_str();
        let mut from = 0;
        while let Some(found) = text.get(from..).and_then(|rest| rest.find(form)) {
            let start = from + found;
            spans.push((start, start + form.len()));
            // On to the next character, so an overlapping occurrence of the
            // same form is found too.
            let step = text
                .get(start..)
                .and_then(|rest| rest.chars().next())
                .map_or(1, char::len_utf8);
            from = start + step;
        }
    }
    spans.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    spans.dedup();
    spans
}

/// Ansible's `_merge_spans`: overlapping and touching spans become one run,
/// and a run is kept when it qualifies. A span qualifies on its own when it
/// is longer than [`MAX_SHORT_CHARS`] or sits at a word boundary; a run of
/// two or more overlapping or touching spans always qualifies; a span lying
/// wholly inside the run qualifies the run when it qualifies on its own.
fn merge(text: &str, spans: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut spans = spans.into_iter();
    let Some((mut run_start, mut run_end)) = spans.next() else {
        return out;
    };
    let mut keep = qualifies(text, run_start, run_end);
    for (start, end) in spans {
        if start > run_end {
            if keep {
                out.push((run_start, run_end));
            }
            (run_start, run_end) = (start, end);
            keep = qualifies(text, start, end);
        } else if end > run_end {
            // Overlapping or touching, and reaching past the run: two
            // secrets side by side are masked whatever their length.
            run_end = end;
            keep = true;
        } else if !keep {
            keep = qualifies(text, start, end);
        }
    }
    if keep {
        out.push((run_start, run_end));
    }
    out
}

/// Whether the occurrence at `start..end` is masked on its own: it is long,
/// or it is a whole word.
fn qualifies(text: &str, start: usize, end: usize) -> bool {
    let long = text
        .get(start..end)
        .is_some_and(|s| s.chars().count() > MAX_SHORT_CHARS);
    let word = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric());
    long || (word(text.get(..start).and_then(|t| t.chars().next_back()))
        && word(text.get(end..).and_then(|t| t.chars().next())))
}

/// `s` inside a JSON string, quotes excluded: `"`, `\` and the control
/// characters escaped as `serde_json` escapes them, and with `ascii` also
/// every character past `~` as `\uXXXX` (a surrogate pair past U+FFFF), as
/// an ASCII-only encoder writes it (Python's `json.dumps` by default). The
/// buffer is sized up front so that it never grows, which would leave a
/// copy behind in the outgrown one.
fn json_escaped(s: &str, ascii: bool) -> Zeroizing<String> {
    // No character takes more than six bytes per byte of its UTF-8.
    let mut out = Zeroizing::new(String::with_capacity(s.len() * 6));
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (ascii && (c as u32) > 0x7e) => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    push_hex(&mut out, *unit);
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// `\u` and four lowercase hex digits.
fn push_hex(out: &mut String, unit: u16) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push_str("\\u");
    for shift in [12, 8, 4, 0] {
        out.push(HEX[usize::from((unit >> shift) & 0xf)] as char);
    }
}

/// `s` as `http::Request::form` writes a key or a value
/// (`application/x-www-form-urlencoded`): unreserved characters as they
/// are, a space as `+`, every other byte `%XX`.
fn form_encoded(s: &str) -> Zeroizing<String> {
    let mut out = Zeroizing::new(String::with_capacity(s.len() * 3));
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'*' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                out.push('%');
                out.push(HEX[usize::from(b >> 4)] as char);
                out.push(HEX[usize::from(b & 0xf)] as char);
            }
        }
    }
    out
}

/// The sink every event passes before it leaves the binary: masks every
/// string in the event, then forwards it. The runtime wraps its top-level
/// sink in one, in every mode, so masking does not depend on which op, tool
/// or playbook line wrote the text.
pub(crate) struct Masking {
    inner: SharedSink,
    registry: Arc<Registry>,
}

impl Masking {
    /// Wrap `inner`, masking with the run's registry.
    pub(crate) fn new(inner: SharedSink) -> Self {
        Masking {
            inner,
            registry: registry(),
        }
    }
}

impl EventSink for Masking {
    fn emit(&self, event: Event) {
        if self.registry.is_empty() {
            return self.inner.emit(event);
        }
        // Masking does not panic (every slice is checked), but if it ever
        // did, the event must not go out as it is: every string of it is
        // replaced instead. The copy kept for that is wiped either way.
        let kept = event.clone();
        let masked = match catch_unwind(AssertUnwindSafe(|| self.registry.mask_event(event))) {
            Ok(masked) => {
                wipe_strings(kept);
                masked
            }
            Err(_) => kept.map_text(&mut |s| {
                drop(Zeroizing::new(s));
                "secret masking failed".to_string()
            }),
        };
        self.inner.emit(masked);
    }
}

/// Wipe every string of `event` rather than free them as they are.
fn wipe_strings(event: Event) {
    let _ = event.map_text(&mut |s| {
        drop(Zeroizing::new(s));
        String::new()
    });
}

/// Install, once per process, a panic hook that masks the panic's message
/// before it is printed to stderr, which `rustible` shows and puts in
/// `--json`. A message with nothing to mask, or a payload that is not text,
/// goes to the hook that was there before (the default one: same text,
/// location and backtrace). One that held a secret is printed the way the
/// default hook prints, with `<secret>` in its place. The runtime installs
/// it before anything is registered.
pub(crate) fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| match masked_panic(info.payload()) {
            None => previous(info),
            Some(message) => print_panic(info, &message),
        }));
    });
}

/// The panic's message with every registered secret masked, when there is
/// one to mask; `None` when the message is not text or holds no secret. A
/// message the registry cannot be read for right now is withheld.
fn masked_panic(payload: &(dyn Any + Send)) -> Option<String> {
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())?;
    match registry().try_mask(message) {
        Some(masked) if masked == message => None,
        Some(masked) => Some(masked),
        None => Some("(message withheld: the secret masker was busy)".to_string()),
    }
}

/// What the default hook prints, with `message` for the panic's own.
fn print_panic(info: &PanicHookInfo<'_>, message: &str) {
    let thread = std::thread::current();
    let name = thread.name().unwrap_or("<unnamed>");
    let at = info
        .location()
        .map(|l| format!(" at {l}"))
        .unwrap_or_default();
    let backtrace = std::backtrace::Backtrace::capture();
    let tail = match backtrace.status() {
        std::backtrace::BacktraceStatus::Captured => format!("stack backtrace:\n{backtrace}"),
        _ => "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n"
            .to_string(),
    };
    eprint!("\nthread '{name}' panicked{at}:\n{message}\n{tail}");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A registry of its own, so each case starts empty.
    fn masked(secrets: &[&str], text: &str) -> String {
        let r = Registry::default();
        for s in secrets {
            r.register(s);
        }
        r.mask(text.to_string())
    }

    const S: &str = PLACEHOLDER;

    // ---- pure: Ansible's corpus (`secret_masking_corpus.json`) ----

    #[test]
    fn a_long_secret_is_masked_anywhere() {
        assert_eq!(masked(&["private"], "AAAprivateBBB"), format!("AAA{S}BBB"));
        assert_eq!(masked(&["1234567"], "AAA1234567BBB"), format!("AAA{S}BBB"));
        assert_eq!(masked(&["private"], "private"), S);
        assert_eq!(
            masked(&["private"], "private a private b private"),
            format!("{S} a {S} b {S}")
        );
        assert_eq!(masked(&["alpha"], "NOTHINGTOSEEHERE"), "NOTHINGTOSEEHERE");
        assert_eq!(masked(&["private"], "PRIVATE data"), "PRIVATE data");
        assert_eq!(masked(&["private"], ""), "");
        assert_eq!(
            masked(&["a.b*c[d]"], "x a.b*c[d] y aXbbcd"),
            format!("x {S} y aXbbcd")
        );
    }

    #[test]
    fn a_short_secret_is_masked_only_as_a_whole_word() {
        assert_eq!(
            masked(&["secret"], "foo secret bar"),
            format!("foo {S} bar")
        );
        assert_eq!(masked(&["secret"], "AAAsecretBBB"), "AAAsecretBBB");
        assert_eq!(masked(&["pass"], "foo pass bar"), format!("foo {S} bar"));
        assert_eq!(masked(&["quick"], "a quick, fox"), format!("a {S}, fox"));
        assert_eq!(masked(&["word"], "AAAwordBBB"), "AAAwordBBB");
        assert_eq!(masked(&["pass"], "pass here"), format!("{S} here"));
        assert_eq!(masked(&["pass"], "type pass"), format!("type {S}"));
        assert_eq!(masked(&["short"], "shortcircuit"), "shortcircuit");
        assert_eq!(masked(&["pass"], "a_pass_b"), format!("a_{S}_b"));
        assert_eq!(masked(&["pass"], "pass"), S);
        assert_eq!(masked(&["pass"], "1pass1"), "1pass1");
        assert_eq!(
            masked(&["pass"], "foo\npass\nbar"),
            format!("foo\n{S}\nbar")
        );
        assert_eq!(masked(&["pass"], "épassé"), "épassé");
        assert_eq!(masked(&["1234"], "pin 1234 ok"), format!("pin {S} ok"));
        assert_eq!(masked(&["1234"], "0012340"), "0012340");
        assert_eq!(
            masked(&["pass"], "pass AAApassBBB"),
            format!("{S} AAApassBBB")
        );
        assert_eq!(
            masked(&["пасс"], "a пасс b, xпассy"),
            format!("a {S} b, xпассy")
        );
        // A password `test` leaves `testing` alone.
        assert_eq!(
            masked(&["test"], "testing the test"),
            format!("testing the {S}")
        );
    }

    #[test]
    fn a_secret_under_four_characters_is_not_registered() {
        assert_eq!(masked(&["abc"], "XX abc XX"), "XX abc XX");
        assert_eq!(masked(&["1"], "count 1"), "count 1");
        // Counted in characters: three astral characters are under four.
        assert_eq!(masked(&["😀😀😀"], "a 😀😀😀 b"), "a 😀😀😀 b");
        assert_eq!(
            masked(&["😀😀😀😀"], "a 😀😀😀😀 b x😀😀😀😀x"),
            format!("a {S} b x😀😀😀😀x")
        );
        let r = Registry::default();
        r.register("ab");
        assert!(r.is_empty());
    }

    #[test]
    fn whitespace_around_a_secret_is_not_part_of_it() {
        assert_eq!(
            masked(
                &["trailing-newline-secret\n"],
                "a=trailing-newline-secret\nb=trailing-newline-secret;c=x"
            ),
            format!("a={S}\nb={S};c=x")
        );
        assert_eq!(
            masked(&[" \t padded-secret-value \r\n"], "x padded-secret-value y"),
            format!("x {S} y")
        );
        assert_eq!(
            masked(&["    \n\t"], "    \n\t indented"),
            "    \n\t indented"
        );
        assert_eq!(masked(&[" ab "], " ab "), " ab ");
        assert_eq!(
            masked(
                &["inner space secret"],
                "inner space secret innerspacesecret"
            ),
            format!("{S} innerspacesecret")
        );
        assert_eq!(
            masked(&[" root "], "root /root/ chroot"),
            format!("{S} /{S}/ chroot")
        );
        // Only space, tab, CR and LF: a form feed stays part of it.
        assert_eq!(
            masked(
                &["\x0cformfeed-secret\x0c"],
                "formfeed-secret \x0cformfeed-secret\x0c"
            ),
            format!("formfeed-secret {S}")
        );
    }

    #[test]
    fn overlapping_and_touching_secrets_are_one_placeholder() {
        assert_eq!(masked(&["pass", "private"], "passprivate"), S);
        assert_eq!(masked(&["abab"], "ababab"), S);
        assert_eq!(masked(&["private", "confiden"], "privateconfiden"), S);
        assert_eq!(
            masked(&["private", "confiden"], "AAAprivateBBBconfidenCCC"),
            format!("AAA{S}BBB{S}CCC")
        );
        assert_eq!(
            masked(&["private", "confiden"], "private confiden"),
            format!("{S} {S}")
        );
        assert_eq!(
            masked(&["hunter2", "ter2foo123"], "start-hunter2foo123-end"),
            format!("start-{S}-end")
        );
        assert_eq!(
            masked(
                &["hunter2", "ter2foo123", "foo123-end"],
                "start-hunter2foo123-end"
            ),
            format!("start-{S}")
        );
        assert_eq!(
            masked(&["hunter2foo", "ter2f"], "x hunter2foo y"),
            format!("x {S} y")
        );
        assert_eq!(masked(&["aaaaaaa"], "xaaaaaaaaay"), format!("x{S}y"));
        assert_eq!(masked(&["private"], "privateprivateprivate"), S);
        assert_eq!(
            masked(&["hunter", "nterfoo123"], "start-hunterfoo123-end"),
            format!("start-{S}-end")
        );
        assert_eq!(masked(&["0000"], "a000000b"), format!("a{S}b"));
        assert_eq!(masked(&["abcd"], "xabcdabcdx"), format!("x{S}x"));
        assert_eq!(masked(&["abcd", "efgh"], "xabcdefghx"), format!("x{S}x"));
        assert_eq!(masked(&["abcd", "efgh"], "xabcd efghx"), "xabcd efghx");
        assert_eq!(
            masked(&["word", "password"], "my password here"),
            format!("my {S} here")
        );
        assert_eq!(
            masked(&["abcdefghij", "bcde"], "x abcdefghij y"),
            format!("x {S} y")
        );
    }

    /// The comments in `_merge_spans`: a span wholly inside a run that does
    /// not qualify qualifies it when it does on its own.
    #[test]
    fn an_inner_span_that_qualifies_qualifies_its_run() {
        assert_eq!(
            masked(&["abcd-x", "abcd"], "xx abcd-xy"),
            format!("xx {S}y")
        );
        assert_eq!(masked(&["x-abcd", "abcd"], "yx-abcd z"), format!("y{S} z"));
        assert_eq!(masked(&["abcdef", "abcde"], "xx abcdefg"), "xx abcdefg");
        assert_eq!(masked(&["abcdef", "bcde"], "xabcdefx"), "xabcdefx");
        assert_eq!(masked(&["abcdef", "abcd"], " abcdefg"), " abcdefg");
    }

    #[test]
    fn secrets_sharing_a_prefix_are_told_apart() {
        let two = ["PREFIX12-a", "PREFIX12-abcd"];
        assert_eq!(masked(&two, "k=PREFIX12-abcd;"), format!("k={S};"));
        assert_eq!(masked(&two, "k=PREFIX12-abc;"), format!("k={S}bc;"));
        assert_eq!(
            masked(
                &["PREFIX12-abcX", "PREFIX12-abcY"],
                "PREFIX12-abcY and PREFIX12-abcZ"
            ),
            format!("{S} and PREFIX12-abcZ")
        );
        assert_eq!(
            masked(&["abcdefgh-MID-1"], "abcdefgh-MID-2 abcdefgh-MID-1x"),
            format!("abcdefgh-MID-2 {S}x")
        );
        assert_eq!(
            masked(&["test\"secret"], "test\"secret2 test\"secre"),
            format!("{S}2 test\"secre")
        );
    }

    #[test]
    fn the_json_forms_are_masked() {
        assert_eq!(
            masked(&["foo\"bar"], r#"{"k": "foo\"bar"}"#),
            format!(r#"{{"k": "{S}"}}"#)
        );
        assert_eq!(
            masked(&["dir\\path-secret"], r#"{"k": "dir\\path-secret"}"#),
            format!(r#"{{"k": "{S}"}}"#)
        );
        assert_eq!(
            masked(
                &["tab\tsecret"],
                "{\"k\": \"tab\\tsecret\"} raw tab\tsecret"
            ),
            format!("{{\"k\": \"{S}\"}} raw {S}")
        );
        // UTF-8, as Rustible writes JSON, and ASCII-only.
        assert_eq!(
            masked(
                &["Zürich-secret"],
                r#"{"a": "Zürich-secret", "b": "Z\u00fcrich-secret"}"#
            ),
            format!(r#"{{"a": "{S}", "b": "{S}"}}"#)
        );
        assert_eq!(
            masked(
                &["key-🔐-secret"],
                r#"{"a": "key-🔐-secret", "b": "key-\ud83d\udd10-secret"}"#
            ),
            format!(r#"{{"a": "{S}", "b": "{S}"}}"#)
        );
        assert_eq!(
            masked(
                &["mix\"ed\\Zü\nrich"],
                "raw:mix\"ed\\Zü\nrich utf8:mix\\\"ed\\\\Zü\\nrich ascii:mix\\\"ed\\\\Z\\u00fc\\nrich"
            ),
            format!("raw:{S} utf8:{S} ascii:{S}")
        );
        // A secret that is another's escaped form is a secret of its own,
        // whose own escaped form is registered too, in either order.
        for pair in [["test\"123", "test\\\"123"], ["test\\\"123", "test\"123"]] {
            assert_eq!(
                masked(&pair, r#"{"a": "test\"123", "b": "test\\\"123"}"#),
                format!(r#"{{"a": "{S}", "b": "{S}"}}"#)
            );
        }
        // And the form is exactly what serde_json writes.
        let tricky = "q\"b\\s/\u{1}\u{8}\u{c}\n\r\t\u{7f}é🔐";
        let json = serde_json::to_string(tricky).unwrap();
        assert_eq!(*json_escaped(tricky, false), json[1..json.len() - 1]);
        assert_eq!(masked(&[tricky], &json), format!("\"{S}\""));
    }

    #[test]
    fn the_form_encoded_form_is_masked() {
        assert_eq!(
            masked(&["p@ss word"], "password=p%40ss+word&x=1"),
            format!("password={S}&x=1")
        );
        assert_eq!(*form_encoded("a b&c=d~"), "a+b%26c%3Dd%7E");
    }

    #[test]
    fn a_secret_over_the_cap_is_registered_as_its_start() {
        let base: String = (0..MAX_CHARS)
            .map(|i| char::from(0x21 + (i % 90) as u8))
            .collect();
        let r = Registry::default();
        r.register(&format!("{base}AAA"));
        r.register(&format!("{base}BBB"));
        assert_eq!(r.0.read().unwrap().secrets.len(), 1);
        assert_eq!(r.mask(format!("{base}CCC")), format!("{S}CCC"));
    }

    #[test]
    fn registering_twice_and_interleaving_keep_every_secret() {
        let r = Registry::default();
        let secrets: Vec<String> = (0..50).map(|i| format!("secret{i:04}xyz")).collect();
        for (i, s) in secrets.iter().enumerate() {
            r.register(s);
            r.register(s);
            let text = secrets[..=i].join(" ");
            assert_eq!(r.mask(text), vec![S; i + 1].join(" "));
        }
    }

    // ---- the sink ----

    /// What `sink` emits for a log line reading `msg`.
    fn through(sink: &Masking, collect: &crate::event::Collect, msg: &str) -> String {
        use crate::event::Level;
        sink.emit(Event::Log {
            level: Level::Info,
            msg: msg.into(),
        });
        match collect.events().pop() {
            Some(Event::Log { msg, .. }) => msg,
            other => panic!("{other:?}"),
        }
    }

    /// Every way a `Secret` is made registers it, and so does the explicit
    /// call.
    #[test]
    fn every_secret_registers_itself() {
        use crate::Secret;
        let collect = Arc::new(crate::event::Collect::default());
        let sink = Masking::new(collect.clone());
        let _ = Secret::new("made-by-new");
        let _ = Secret::from("made-from-string".to_string());
        let _: Secret = serde_json::from_str("\"made-by-deserialize\"").unwrap();
        let _ = Secret::new(b"not-utf8-\xff-secret".to_vec());
        register("registered-by-hand");
        for s in [
            "made-by-new",
            "made-from-string",
            "made-by-deserialize",
            "registered-by-hand",
        ] {
            assert_eq!(
                through(&sink, &collect, &format!("a {s} b")),
                "a <secret> b"
            );
        }
        // Not UTF-8: as a lossy decoding shows it.
        let lossy = String::from_utf8_lossy(b"not-utf8-\xff-secret").into_owned();
        assert_eq!(through(&sink, &collect, &lossy), "<secret>");
    }

    /// The panic hook's decision: a text message holding a secret is
    /// printed masked, any other goes to the previous hook untouched, and
    /// one that cannot be masked right now is withheld rather than printed.
    #[test]
    fn a_panic_message_is_masked_only_when_it_holds_a_secret() {
        let payload = |p: &(dyn Any + Send)| masked_panic(p);
        register("s3cret-in-a-panic");
        assert_eq!(
            payload(&String::from("token s3cret-in-a-panic rejected")),
            Some("token <secret> rejected".to_string())
        );
        assert_eq!(
            payload(&"token s3cret-in-a-panic"),
            Some("token <secret>".into())
        );
        assert_eq!(payload(&String::from("nothing secret")), None);
        assert_eq!(payload(&42u8), None, "not text: the previous hook's");
        let held = registry();
        let _writing = held.0.write().unwrap();
        let withheld = payload(&String::from("s3cret-in-a-panic")).unwrap();
        assert!(withheld.contains("withheld"), "{withheld}");
    }

    /// Tests run in parallel in one process: a secret another test (another
    /// thread) registers is not masked in this one's output.
    #[test]
    fn a_secret_registered_on_another_thread_does_not_mask_this_ones_output() {
        let collect = Arc::new(crate::event::Collect::default());
        let sink = Masking::new(collect.clone());
        std::thread::spawn(|| register("another-tests-secret"))
            .join()
            .unwrap();
        assert_eq!(
            through(&sink, &collect, "another-tests-secret"),
            "another-tests-secret"
        );
        register("this-tests-secret");
        assert_eq!(through(&sink, &collect, "this-tests-secret"), "<secret>");
    }

    // ---- apply_prefix ----

    /// `apply_prefix` over `text` as a stream gives it: exactly the bytes
    /// asked for, no more, so a test fails if the masker asks for too few.
    fn prefix_of(text: &str, max: usize) -> (String, bool) {
        apply_prefix(max, |want| {
            assert!(want >= max, "asked for {want} bytes, under {max}");
            text.get(..want.min(text.len())).unwrap().to_string()
        })
    }

    /// A secret across the cut is masked whole, never shown in part: one
    /// placeholder, kept whole, ends the start.
    #[test]
    fn a_secret_straddling_the_cut_is_masked_whole() {
        // Longer than the lookahead alone: the masker must read past `max`
        // by the longest form to see it whole.
        let secret = &format!("straddling-s3cret-{}", "v".repeat(60));
        register(secret);
        let max = 128;
        for before in max - secret.len() + 1..max {
            let text = format!("{}{secret}{}", "x".repeat(before), "y".repeat(1000));
            let (start, cut) = prefix_of(&text, max);
            assert!(cut);
            assert_eq!(start, format!("{}{S}", "x".repeat(before)), "{before}");
        }
        // Ending exactly at the end of the text: masked, and not cut.
        let text = format!("{}{secret}", "x".repeat(50));
        assert_eq!(
            prefix_of(&text, 60),
            (format!("{}{S}", "x".repeat(50)), false)
        );
        // Past the cut entirely: not reached, nothing of it shown.
        let text = format!("{}{secret}", "x".repeat(max));
        assert_eq!(prefix_of(&text, max), ("x".repeat(max), true));
    }

    /// A short secret on the cut is masked when a long one touches it past
    /// the cut, as it would be in the whole text: the masker looks far
    /// enough to see the long one.
    #[test]
    fn a_short_secret_on_the_cut_is_qualified_by_one_touching_it() {
        register("abcdef");
        register("LONG-SECRET-TOUCHING");
        let max = 40;
        let text = format!(
            "{}zabcdefLONG-SECRET-TOUCHING{}",
            "-".repeat(max - 2),
            "-".repeat(500)
        );
        let (start, cut) = prefix_of(&text, max);
        assert_eq!(start, format!("{}z{S}", "-".repeat(max - 2)));
        assert!(cut);
        // Inside a word and alone, it is not masked.
        let text = format!("{}zabcdefz{}", "-".repeat(max - 2), "-".repeat(500));
        assert_eq!(
            prefix_of(&text, max).0,
            format!("{}za", "-".repeat(max - 2))
        );
        // Six 4-byte characters, the first before the cut, touched by the
        // longest form: the masker must look past the cut by the short
        // one's whole length too. (The long one outgrows the emoji's
        // ASCII-escaped form, 72 bytes, so `longest` alone does not cover
        // it.)
        let long = "L".repeat(100);
        register(&long);
        register("😀😀😀😀😀😀");
        let text = format!(
            "{}z😀😀😀😀😀😀{long}{}",
            "-".repeat(max - 5),
            "-".repeat(500)
        );
        assert_eq!(
            prefix_of(&text, max).0,
            format!("{}z{S}", "-".repeat(max - 5))
        );
    }

    /// A masked run reaching the end of what was read still reports a cut
    /// when the text goes on.
    #[test]
    fn a_run_reaching_the_end_of_what_was_read_is_still_a_cut() {
        let s = "w".repeat(44);
        register(&s);
        let max = 100;
        let text = format!("{}{s}{s}{}", "x".repeat(max - 16), "y".repeat(100));
        assert_eq!(
            prefix_of(&text, max),
            (format!("{}{S}", "x".repeat(max - 16)), true)
        );
        // And plain text cut at `max`, or a placeholder ending the text, is
        // reported as cut or whole as it is.
        assert_eq!(prefix_of("abcdef", 3), ("abc".to_string(), true));
        assert_eq!(
            prefix_of(&format!("ab {s}"), 10),
            (format!("ab {S}"), false)
        );
    }

    /// Whatever the cut, a placeholder is whole or absent, the result runs
    /// past `max` only by a placeholder that starts before it, and no
    /// character of the secret shows.
    #[test]
    fn a_placeholder_is_never_split() {
        register("pl4ceholder-secret");
        let text = "ab pl4ceholder-secret cd pl4ceholder-secret";
        for max in 0..=text.len() + 2 {
            let (start, cut) = prefix_of(text, max);
            assert!(
                start.len() <= max || start.ends_with(S) && start.len() < max + S.len(),
                "{max}: {start}"
            );
            assert!(!start.contains("pl4"), "{max}: {start}");
            let opened = start.matches('<').count();
            assert_eq!(opened, start.matches(S).count(), "{max}: {start}");
            assert_eq!(cut, start != format!("ab {S} cd {S}"), "{max}: {start}");
        }
        // Cut on a character boundary, too.
        assert_eq!(prefix_of("ééé", 3), ("é".to_string(), true));
    }

    /// A secret under 4 characters is not registered, so the start is the
    /// text's own.
    #[test]
    fn apply_prefix_leaves_a_secret_under_four_characters() {
        register("abc");
        assert_eq!(prefix_of("x abc y", 100), ("x abc y".to_string(), false));
        assert_eq!(prefix_of("x abc y", 5), ("x abc".to_string(), true));
    }

    /// Nothing registered: the start is the text cut at `max`, as it came.
    #[test]
    fn apply_prefix_with_nothing_registered_only_cuts() {
        std::thread::spawn(|| {
            assert!(registry().is_empty());
            assert_eq!(
                prefix_of("hello <secret> world", 100),
                ("hello <secret> world".to_string(), false)
            );
            assert_eq!(prefix_of("hello world", 5), ("hello".to_string(), true));
            assert_eq!(prefix_of("hello", 5), ("hello".to_string(), false));
            assert_eq!(prefix_of("", 5), (String::new(), false));
        })
        .join()
        .unwrap();
    }

    /// The registry is the thread's under `testing`, as the sink's is: a
    /// secret another test registers does not mask this one's text.
    #[test]
    fn apply_prefix_sees_only_this_threads_secrets() {
        std::thread::spawn(|| register("other-threads-secret"))
            .join()
            .unwrap();
        let text = "a other-threads-secret b this-threads-secret c";
        assert_eq!(prefix_of(text, 100), (text.to_string(), false));
        register("this-threads-secret");
        assert_eq!(
            prefix_of(text, 100),
            (format!("a other-threads-secret b {S} c"), false)
        );
    }

    #[test]
    fn the_sink_masks_every_string_of_an_event() {
        use crate::diff::{AttrChange, Diff};
        use crate::error::CmdFailed;
        use crate::event::{Collect, Status};

        let collect = Arc::new(Collect::default());
        let sink = Masking::new(collect.clone());
        register("hunter2-in-a-sink");
        let s = "x hunter2-in-a-sink y";
        sink.emit(Event::StepFinished {
            id: 1,
            blocks: vec![s.into()],
            name: s.into(),
            identity: s.into(),
            status: Status::Failed,
            diff: Diff::many(vec![
                Diff::text(s, s, s),
                Diff::attrs(s, vec![AttrChange::new(s, s, s)]),
                Diff::summary(s),
            ]),
            note: Some(s.into()),
            elapsed_ms: 0,
            cmd: Some(CmdFailed {
                argv: vec![s.into()],
                status: 1,
                signal: None,
                stderr: s.into(),
            }),
        });
        let json = serde_json::to_string(&collect.events()).unwrap();
        assert!(!json.contains("hunter2"), "{json}");
        assert_eq!(json.matches("x <secret> y").count(), 14, "{json}");
    }
}
