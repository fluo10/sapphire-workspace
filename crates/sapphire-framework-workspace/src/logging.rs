//! The app's log: a file inside the application's data directory.
//!
//! Every sapphire app server installs a `tracing` layer that writes the framework's and
//! the app's own targets to `<data dir>/logs/app.log`, on top of what the process already
//! prints. The app server is service-managed and single-instance per app per host, so one
//! process writes one file: it is a continuous record across restarts rather than an
//! interleaving. The file rotates at [`LOG_MAX_BYTES`], and [`LOG_KEEP`] rotated files are
//! kept.
//!
//! The subscriber itself is installed by [`install_console`], which [`AppContext::init`]
//! calls — so an app gets logging by building its context, with no wiring of its own.
//! [`install`] then routes the file layer at the app's log directory, and the guard it
//! returns is held for as long as the server serves.
//!
//! This is the same shape the bridge's own `logging` module has always had, lifted onto
//! the shared foundation so every app server gets it for free.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::{EnvFilter, Targets};
use tracing_subscriber::fmt::writer::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::context::AppContext;
use crate::error::Result;

/// The log file's name, inside the app data directory's log directory.
pub const LOG_FILE: &str = "app.log";

/// The current log file is rotated once it holds this many bytes.
pub const LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// How many rotated files are kept, in addition to the current one.
pub const LOG_KEEP: usize = 3;

/// The framework targets whose events the log file keeps.
///
/// The crates an app server runs — the server itself (whose `sync` module carries the
/// sync runtime's target), the sync core, and the workspace layer. The app's own target
/// joins them ([`app_target`]); anything else the process logs — its dependencies — stays
/// out of the file, so it answers "what did this app do".
pub const FRAMEWORK_TARGETS: [&str; 3] = [
    "sapphire_framework_server",
    "sapphire_framework_sync",
    "sapphire_framework_workspace",
];

/// How often a following log reader looks for more lines.
const FOLLOW_POLL: Duration = Duration::from_millis(250);

/// The log filter a process applies when the environment asks for nothing specific:
/// the framework's targets and the app's own, at info.
///
/// The same default the console layer and the file layer agree on, so a server's journald
/// output and its log file carry the same lines without `RUST_LOG` being set.
pub fn default_log_filter(app_name: &str) -> String {
    FRAMEWORK_TARGETS
        .iter()
        .map(|target| format!("{target}=info"))
        .collect::<Vec<_>>()
        .join(",")
        + &format!(",{}=info", app_target(app_name))
}

/// The `tracing` target an application logs under: its app name with dashes as
/// underscores, which is what `tracing` derives from a crate path.
///
/// An application whose library crate is named differently (a `-core` split, say) should
/// log with an explicit `target:` if it wants its lines in the file.
pub fn app_target(app_name: &str) -> String {
    app_name.replace('-', "_")
}

/// Where the file layer's events go right now.
///
/// `tracing` installs a global subscriber once per process and never replaces it, but the
/// file layer may be installed twice in one process's lifetime — a restart inside a
/// supervisor, and the tests. So the subscriber is built once, around this wire, and every
/// [`install`] swaps which writer the wire feeds. `None` means nothing holds the log, and
/// file events go nowhere while the console layer keeps printing.
type Wire = Arc<RwLock<Option<Arc<NonBlocking>>>>;

/// Where the file layer's events go right now.
static WIRE: OnceLock<Wire> = OnceLock::new();

/// Install the process's subscriber: the console layer, plus the file layer behind
/// [`WIRE`] once [`install`] routes a directory into it.
///
/// [`AppContext::init`] calls this, so every app that initialises its context prints
/// through it. If a global subscriber got set first — an embedding application's own
/// choice — the attempt is given up quietly and the file layer simply never hears an
/// event; the wire still exists, so [`install`] keeps working for a caller that wants a
/// writer anyway.
pub fn install_console(app_name: &'static str) {
    WIRE.get_or_init(|| build_subscriber(app_name));
}

/// Route the app's log file for `ctx`'s data directory, rotating at [`LOG_MAX_BYTES`].
///
/// What installing means:
/// - the file `<data dir>/logs/app.log` receives every event of the framework's and the
///   app's own targets, in addition to whatever the process prints;
/// - the file is opened for appending, so a restarted server continues the same record;
/// - events reach the file through a writer thread, and the returned [`LogGuard`] flushes
///   it when dropped. Keep the guard for as long as the server serves.
///
/// Installing a second time — a server restarting inside this process — re-routes the
/// file layer to the new writer; the subscriber itself is built once.
pub fn install(ctx: &AppContext) -> Result<LogGuard> {
    install_with_limit(ctx, LOG_MAX_BYTES)
}

/// As [`install`], rotating at `limit` bytes rather than [`LOG_MAX_BYTES`].
///
/// A file that passes the limit is renamed aside — `app.log.1`, `app.log.2` and so on —
/// keeping [`LOG_KEEP`] rotated files and starting a fresh current one. Rotation is
/// approximate: a file may overshoot `limit` by as much as one event.
pub fn install_with_limit(ctx: &AppContext, limit: u64) -> Result<LogGuard> {
    // Build the subscriber once; every later install only re-routes the file layer. A
    // caller that never ran `install_console` gets the subscriber here.
    let wire = WIRE.get_or_init(|| build_subscriber(ctx.app_name));

    // A context whose data directory was never resolved has nowhere to put a log:
    // the file layer keeps draining events — to a sink — but nothing lands on disk.
    let (writer, worker) = match ctx.try_log_dir() {
        Some(dir) => {
            std::fs::create_dir_all(&dir)?;
            tracing_appender::non_blocking(Rotating::open(dir.join(LOG_FILE), limit, LOG_KEEP))
        }
        None => tracing_appender::non_blocking(std::io::sink()),
    };
    let writer = Arc::new(writer);

    *wire
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&writer));
    Ok(LogGuard {
        wire: Arc::clone(wire),
        writer,
        _worker: worker,
    })
}

/// Build the subscriber the process prints through: the console layer, and the file layer
/// routing through [`WIRE`].
fn build_subscriber(app_name: &'static str) -> Wire {
    let wire: Wire = Arc::new(RwLock::new(None));

    // The console layer is built here rather than left to the caller: this subscriber is
    // the one the process prints through, and a server run must not lose the console
    // output journald would otherwise show.
    let console = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stdout)
        .with_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(default_log_filter(app_name))),
        );
    let file = tracing_subscriber::fmt::layer()
        // A file any process reads wants text, not terminal escapes.
        .with_ansi(false)
        .with_writer(FileLayer {
            wire: Arc::clone(&wire),
        })
        .with_filter(
            Targets::new()
                .with_target(
                    app_target(app_name),
                    tracing::level_filters::LevelFilter::TRACE,
                )
                .with_targets(
                    FRAMEWORK_TARGETS
                        .iter()
                        .map(|target| (*target, tracing::level_filters::LevelFilter::TRACE)),
                ),
        );

    let _ = tracing_subscriber::registry()
        .with(console)
        .with(file)
        .try_init();
    wire
}

/// The file layer's writer: whatever [`install`] last routed through [`WIRE`].
#[derive(Clone, Debug)]
struct FileLayer {
    wire: Wire,
}

impl<'a> MakeWriter<'a> for FileLayer {
    type Writer = RoutedWriter;

    fn make_writer(&'a self) -> Self::Writer {
        let current = self
            .wire
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        RoutedWriter {
            writer: current.map(|routed| (*routed).clone()),
        }
    }
}

/// A writer holding the log writer that was current when the event was made.
#[derive(Debug)]
struct RoutedWriter {
    writer: Option<NonBlocking>,
}

impl Write for RoutedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match &mut self.writer {
            Some(writer) => writer.write(buf),
            // Nothing holds the log: the event was never meant for a file.
            None => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match &mut self.writer {
            Some(writer) => writer.flush(),
            None => Ok(()),
        }
    }
}

/// Keeps the log's writer thread alive, and flushes it when dropped.
///
/// Dropping it unhooks the wire first — so later events no longer queue into a worker that
/// is about to stop — and then shuts the worker down, flushing everything queued. A guard
/// is meant to bracket a server's lifetime: a newer [`install`] re-routes the wire, and an
/// older guard's drop leaves that routing alone.
#[derive(Debug)]
pub struct LogGuard {
    wire: Wire,
    writer: Arc<NonBlocking>,
    _worker: WorkerGuard,
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        let mut wire = self
            .wire
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A newer install may have re-routed the wire already; only unhook our own.
        if wire
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &self.writer))
        {
            *wire = None;
        }
    }
}

/// The last `lines` lines of the app's log, in order.
///
/// A log that is not there yet is an empty tail rather than an error: the app has never
/// run a server on this host, which is a normal thing for a log reader to report. Only
/// the current file is read — lines an earlier rotation moved into `app.log.1` are the
/// rotated files' business.
pub fn tail(ctx: &AppContext, lines: usize) -> Result<Vec<String>> {
    let bytes = match std::fs::read(ctx.log_dir().join(LOG_FILE)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    Ok(last_lines(&bytes, lines))
}

/// Print the log's tail, then keep printing as the file grows, until `stop` is true.
///
/// `tail -f`, spelled out: the last `lines` lines go out at once, then each poll prints
/// what appeared since. A file that shrank — rotated or truncated under us — is read from
/// its start again, so following a rotating log keeps printing rather than stalling.
pub fn follow(
    ctx: &AppContext,
    lines: usize,
    out: &mut impl Write,
    stop: impl Fn() -> bool,
) -> Result<()> {
    let path = ctx.log_dir().join(LOG_FILE);
    // The tail first, then continue from where it ended: the offset is the length of what
    // was read, so nothing between the read and the follow is printed twice.
    let mut offset = match std::fs::read(&path) {
        Ok(bytes) => {
            for line in last_lines(&bytes, lines) {
                writeln!(out, "{line}")?;
            }
            out.flush()?;
            bytes.len() as u64
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(e.into()),
    };

    loop {
        print_growth(&path, &mut offset, out)?;
        // Checked only after the drain, so a stop set while lines were landing still
        // prints them rather than ending one poll early.
        if stop() {
            return Ok(());
        }
        std::thread::sleep(FOLLOW_POLL);
    }
}

/// Print the bytes that appeared past `offset`, and leave `offset` at the new end.
fn print_growth(path: &Path, offset: &mut u64, out: &mut impl Write) -> Result<()> {
    let len = match std::fs::metadata(path) {
        Ok(meta) => meta.len(),
        // The file is being rotated, or was removed: the next poll finds whatever took
        // its place, and the record to follow starts over from its first byte.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            *offset = 0;
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    // The file shrank: it was rotated or truncated. Whatever we had read is gone from
    // this name, and the record to follow starts over.
    if len < *offset {
        *offset = 0;
    }
    if len == *offset {
        return Ok(());
    }
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(*offset))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    *offset = len;
    out.write_all(&bytes)?;
    out.flush()?;
    Ok(())
}

/// The last `lines` lines of `bytes`, in order, without their newlines.
///
/// Bytes after the last newline are a line still being written and are reported: a tail
/// that swallows them would hide what the server is saying right now.
fn last_lines(bytes: &[u8], lines: usize) -> Vec<String> {
    if lines == 0 {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(bytes);
    let collected: Vec<&str> = text.lines().collect();
    let start = collected.len().saturating_sub(lines);
    collected[start..]
        .iter()
        .map(|line| (*line).to_owned())
        .collect()
}

/// A log file that rotates itself once it passes a size limit.
///
/// Only the app's server writes this file — the single-instance discipline sees to it —
/// so the writer needs no locking against a second process, only against its own
/// rotation. The count of bytes written travels with the open file, so no write has to
/// ask the filesystem how big the file is. The writer thread of `tracing-appender` is
/// this file's only caller.
struct Rotating {
    path: PathBuf,
    limit: u64,
    keep: usize,
    state: Mutex<Option<Open>>,
}

/// One open log file, and how much it already holds.
struct Open {
    file: std::fs::File,
    written: u64,
}

impl Rotating {
    /// Open (or create) the log at `path`, rotating at `limit` and keeping `keep` rotated
    /// files.
    fn open(path: PathBuf, limit: u64, keep: usize) -> Rotating {
        Rotating {
            path,
            limit,
            keep,
            state: Mutex::new(None),
        }
    }

    /// Retire the oldest rotated file, shift the rest up, and open a fresh current file.
    fn rotate(&self, state: &mut Option<Open>) -> std::io::Result<()> {
        // The open file's name is about to change under it: drop the handle first, shift
        // the rotated names up, and open fresh, so the next bytes land in the new file.
        *state = None;
        let rotated = |n: usize| self.path.with_extension(format!("log.{n}"));
        for n in (1..self.keep).rev() {
            rename_away(&rotated(n), &rotated(n + 1))?;
        }
        rename_away(&self.path, &rotated(1))?;
        *state = Some(Open {
            file: open_append(&self.path)?,
            written: 0,
        });
        Ok(())
    }
}

/// Rename `from` to `to`, ignoring a `from` that is not there.
fn rename_away(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Open `path` for appending, creating it if absent.
///
/// Appending is what makes a restart continue the same record: no truncate, no rewrite.
fn open_append(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
}

impl Write for Rotating {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut state = self.state.lock().expect("rotating log state");
        if state.is_none() {
            // The file may already hold a record from earlier runs; the first write of a
            // restart continues it, which is why the count starts at what is on disk.
            let written = std::fs::metadata(&self.path).map_or(0, |m| m.len());
            let file = open_append(&self.path)?;
            *state = Some(Open { file, written });
        }
        let full = state
            .as_ref()
            .is_some_and(|open| open.written + buf.len() as u64 > self.limit);
        if full {
            self.rotate(&mut state)?;
        }
        let open = state.as_mut().expect("rotation left no file open");
        open.file.write_all(buf)?;
        open.written += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut state = self.state.lock().expect("rotating log state");
        if let Some(open) = state.as_mut() {
            open.file.flush()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The logging tests install the process's one global subscriber, and `cargo test`
    /// runs a binary's tests in parallel threads: one lock takes them turns, so one
    /// test's events never land in another test's file.
    static INSTALL_LOCK: Mutex<()> = Mutex::new(());

    /// A context whose data directory is a scratch tree. The tree and the context are
    /// leaked to `'static` because [`install`] takes the process-wide context the way
    /// every other call site does.
    fn scratch_ctx(name: &'static str) -> &'static AppContext {
        let tmp = Box::leak::<'static>(Box::new(tempfile::TempDir::new().unwrap()));
        let ctx: &'static AppContext = Box::leak(Box::new(AppContext::new(name)));
        ctx.set_data_dir(tmp.path().to_path_buf());
        ctx
    }

    #[test]
    fn installing_creates_the_log_file() {
        let _turn = INSTALL_LOCK.lock().unwrap();
        let ctx = scratch_ctx("saphtestlog-a");
        let guard = install(ctx).unwrap();
        tracing::info!(target: "sapphire_framework_server", "hello from the test");
        drop(guard);

        let text = std::fs::read_to_string(ctx.log_dir().join(LOG_FILE)).unwrap();
        assert!(text.contains("hello from the test"), "{text}");
    }

    #[test]
    fn the_log_rotates_at_its_size_limit() {
        let _turn = INSTALL_LOCK.lock().unwrap();
        let ctx = scratch_ctx("saphtestlog-b");
        let guard = install_with_limit(ctx, 4096).unwrap();
        for n in 0..2000 {
            tracing::info!(target: "sapphire_framework_server", "line {n} padded {}", "x".repeat(64));
        }
        drop(guard);

        let files: Vec<String> = std::fs::read_dir(ctx.log_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(files.len() > 1, "the log never rotated: {files:?}");
        assert!(files.len() <= LOG_KEEP + 1, "too many kept: {files:?}");
    }

    #[test]
    fn a_restart_continues_the_same_file() {
        let _turn = INSTALL_LOCK.lock().unwrap();
        let ctx = scratch_ctx("saphtestlog-c");

        let guard = install(ctx).unwrap();
        tracing::info!(target: "sapphire_framework_server", "first run");
        drop(guard);

        let guard = install(ctx).unwrap();
        tracing::info!(target: "sapphire_framework_server", "second run");
        drop(guard);

        let text = std::fs::read_to_string(ctx.log_dir().join(LOG_FILE)).unwrap();
        assert!(
            text.contains("first run") && text.contains("second run"),
            "{text}"
        );
    }

    #[test]
    fn reading_the_tail_of_a_missing_log_is_not_an_error() {
        let ctx = scratch_ctx("saphtestlog-d");
        assert!(tail(ctx, 20).unwrap().is_empty());
    }

    #[test]
    fn the_tail_returns_the_last_lines_in_order() {
        let ctx = scratch_ctx("saphtestlog-e");
        std::fs::create_dir_all(ctx.log_dir()).unwrap();
        std::fs::write(
            ctx.log_dir().join(LOG_FILE),
            (0..100).map(|n| format!("line {n}\n")).collect::<String>(),
        )
        .unwrap();

        let lines = tail(ctx, 3).unwrap();
        assert_eq!(lines, vec!["line 97", "line 98", "line 99"]);
    }

    #[test]
    fn following_prints_the_lines_that_arrive() {
        let ctx = scratch_ctx("saphtestlog-f");
        let log = ctx.log_dir().join(LOG_FILE);
        std::fs::create_dir_all(ctx.log_dir()).unwrap();
        std::fs::write(&log, "line 0\nline 1\nline 2\n").unwrap();

        // Append more lines a moment later, then raise the flag that ends the follow.
        let arrived = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&arrived);
        let path = log.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(b"line 3\nline 4\n").unwrap();
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        // A deadline keeps a failed append from hanging the test: the stop fires either
        // way, and the assertions below tell which.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let flag = Arc::clone(&arrived);
        let stop = move || {
            flag.load(std::sync::atomic::Ordering::SeqCst) || std::time::Instant::now() > deadline
        };

        let mut out: Vec<u8> = Vec::new();
        follow(ctx, 10, &mut out, stop).unwrap();
        writer.join().unwrap();

        let text = String::from_utf8(out).unwrap();
        for line in ["line 0", "line 1", "line 2", "line 3", "line 4"] {
            assert!(text.contains(line), "missing {line}: {text}");
        }
        let (first, last) = (text.find("line 0").unwrap(), text.find("line 4").unwrap());
        assert!(
            first < last,
            "the tail and the growth came out of order: {text}"
        );
    }
}
