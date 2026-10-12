//! Logging (`util/src/observability/logging.ts`): logfmt-like lines appended to a shared file,
//! mirrored to stderr when `OPENCODE_PRINT_LOGS=1`, trimmed in place above 50 MiB.

use std::fmt::Write as _;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

pub fn iso_now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

fn level_name(l: &Level) -> &'static str {
    match *l {
        Level::TRACE => "TRACE",
        Level::DEBUG => "DEBUG",
        Level::INFO => "INFO",
        Level::WARN => "WARN",
        Level::ERROR => "ERROR",
    }
}

/// Quote a value unless it matches `^[^\s="\\]+$`.
pub fn encode(value: &str) -> String {
    if !value.is_empty() && !value.chars().any(|c| c.is_whitespace() || c == '=' || c == '"' || c == '\\') {
        value.to_string()
    } else {
        serde_json::to_string(value).unwrap()
    }
}

#[derive(Default)]
struct Fields(String);

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        let _ = write!(self.0, " {}={}", field.name(), encode(value));
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, " {}={}", field.name(), encode(&format!("{value:?}")));
    }
}

struct Sink {
    file: Option<std::fs::File>,
}

pub struct LogfmtLayer {
    run: String,
    sink: Arc<Mutex<Sink>>,
    annotations: String,
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for LogfmtLayer {
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let mut line = format!(
            "timestamp={} level={} run={}{}",
            iso_now(),
            level_name(event.metadata().level()),
            self.run,
            fields.0
        );
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                if let Some(f) = span.extensions().get::<Fields>() {
                    line.push_str(&f.0);
                }
            }
        }
        line.push_str(&self.annotations);
        line.push('\n');
        if std::env::var("OPENCODE_PRINT_LOGS").as_deref() == Ok("1") {
            let _ = std::io::stderr().write_all(line.as_bytes());
        }
        if let Ok(mut sink) = self.sink.lock() {
            if let Some(f) = sink.file.as_mut() {
                let _ = f.write_all(line.as_bytes());
            }
        }
    }

    fn on_new_span(&self, attrs: &tracing::span::Attributes<'_>, id: &tracing::span::Id, ctx: Context<'_, S>) {
        let mut f = Fields::default();
        attrs.record(&mut f);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(f);
        }
    }
}

fn parse_level(v: &str) -> Option<tracing::level_filters::LevelFilter> {
    use tracing::level_filters::LevelFilter as F;
    Some(match v.to_ascii_lowercase().as_str() {
        "all" | "trace" => F::TRACE,
        "debug" => F::DEBUG,
        "info" => F::INFO,
        "warn" | "warning" => F::WARN,
        "error" | "fatal" => F::ERROR,
        "none" => F::OFF,
        _ => return None,
    })
}

/// Install the global logger. `role` is added as an annotation (`role=cli`, `role=server`).
pub fn init(file: &Path, role: &str, level: Option<&str>) {
    let run = uuid::Uuid::new_v4().to_string()[..8].to_string();
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let handle = std::fs::OpenOptions::new().create(true).append(true).open(file).ok();
    let filter = level
        .and_then(parse_level)
        .or_else(|| std::env::var("OPENCODE_LOG_LEVEL").ok().as_deref().and_then(parse_level))
        .unwrap_or(tracing::level_filters::LevelFilter::INFO);
    let layer = LogfmtLayer {
        run,
        sink: Arc::new(Mutex::new(Sink { file: handle })),
        annotations: format!(" role={}", encode(role)),
    };
    let _ = tracing_subscriber::registry().with(layer.with_filter(filter)).try_init();
    let target = file.to_path_buf();
    std::thread::Builder::new()
        .name("log-trim".into())
        .spawn(move || loop {
            let _ = trim(&target);
            std::thread::sleep(Duration::from_secs(3600));
        })
        .ok();
}

const MAX: u64 = 50 * 1024 * 1024;
const KEEP: u64 = 25 * 1024 * 1024;

/// In-place trim (`logging.ts` `trim`).
pub fn trim(target: &Path) -> std::io::Result<()> {
    let lock = PathBuf::from(format!("{}.trim", target.display()));
    if std::fs::create_dir(&lock).is_err() {
        if let Ok(m) = std::fs::metadata(&lock).and_then(|m| m.modified()) {
            if m.elapsed().unwrap_or_default() > Duration::from_secs(300) {
                let _ = std::fs::remove_dir_all(&lock);
            }
        }
        return Ok(());
    }
    let result = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new().read(true).write(true).open(target)?;
        let size = f.metadata()?.len();
        if size <= MAX {
            return Ok(());
        }
        let mut pos = size - KEEP;
        f.seek(SeekFrom::Start(pos))?;
        let mut buf = vec![0u8; 64 * 1024];
        'find: loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            for (i, b) in buf[..n].iter().enumerate() {
                if *b == b'\n' {
                    pos += i as u64 + 1;
                    break 'find;
                }
            }
            pos += n as u64;
        }
        let mut read = pos;
        let mut written = 0u64;
        loop {
            f.seek(SeekFrom::Start(read))?;
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            f.seek(SeekFrom::Start(written))?;
            f.write_all(&buf[..n])?;
            read += n as u64;
            written += n as u64;
        }
        f.set_len(written)
    })();
    let _ = std::fs::remove_dir_all(&lock);
    result
}
