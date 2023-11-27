//! Macros for tracing queue behavior
use anyhow::Result;
use const_format::formatcp;
use paste::paste;
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tracing::field::{Field, Visit};
use tracing::Level;
use tracing::{Event, Subscriber};
use tracing_subscriber::filter::{filter_fn, EnvFilter};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

/// Default directive passed via EnvFilter semantics
pub const DEFAULT_FILTER_DIRECTIVE: &str = formatcp!("{}=off", TARGET);

/// Re-exported tracing so queue macros expand properly at callsite
#[doc(hidden)]
pub use tracing;

/// Target value for all queue tracing events
///
/// This value can be used with the
/// [`EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html)
/// to enable or disable queue tracing
pub const TARGET: &str = "queue";

#[doc(hidden)]
pub const NENQ_FIELD_ID: &str = "nenq";
#[doc(hidden)]
pub const NDEQ_FIELD_ID: &str = "ndeq";
#[doc(hidden)]
pub const DISTINGUISHER_FIELD_ID: &str = "distinguisher";

#[doc(hidden)]
pub struct QueueInner {
    #[doc(hidden)]
    pub name: &'static str,
    #[doc(hidden)]
    pub level: Level,
}

/// Tracking type for queues. Created with the *_queue macros.
pub struct Queue(#[doc(hidden)] pub QueueInner);

/// Tracking type for queue groups. Created with the *_queue_group macros.
pub struct QueueGroup(#[doc(hidden)] pub QueueInner);

macro_rules! queue_with_level {
    ($level:ident) => {
        paste! {
            #[doc = concat!("Declare a queue with a unique name at the ", stringify!($level), " level")]
            #[doc(hidden)]
            #[macro_export]
            macro_rules! [< __ $level:lower _queue__ >] {
                ($name:literal) => {{
                    $crate::queue::Queue(
                        $crate::queue::QueueInner {
                            name: $name,
                            level: $crate::Level::[< $level:upper >]
                        }
                    )
                }}
            }

            #[doc(inline)]
            pub use [< __ $level:lower _queue__ >] as [< $level:lower _queue >];

            #[doc = concat!("Declare a queue group with a unique name at the ", stringify!($level), " level")]
            #[doc(hidden)]
            #[macro_export]
            macro_rules! [< __ $level:lower _queue_group__ >] {
                ($name:literal) => {{
                    $crate::queue::QueueGroup(
                        $crate::queue::QueueInner {
                            name: $name,
                            level: $crate::Level::[< $level:upper >]
                        }
                    )
                }}
            }

            #[doc(inline)]
            pub use [< __ $level:lower _queue_group__ >] as [< $level:lower _queue_group >];
        }
    };
}

queue_with_level!(trace);
queue_with_level!(debug);
queue_with_level!(info);
queue_with_level!(warn);
queue_with_level!(error);

/// Enqueue 1 or an optionally provided amount of items to the given queue
#[doc(hidden)]
#[macro_export]
macro_rules! __enq__ {
    ($queue:expr) => {{
        $crate::queue::enq!($queue, 1);
    }};
    ($queue:expr, $nenq:expr) => {{
        const queue: $crate::queue::Queue = $queue;
        $crate::queue::tracing::span!(
            target: $crate::queue::TARGET,
            queue.0.level,
            queue.0.name,
        ).in_scope(|| {
            $crate::queue::tracing::event!(
                target: $crate::queue::TARGET,
                queue.0.level,
                { $crate::queue::NENQ_FIELD_ID } = $nenq as u64,
            );
        });
    }};
}
#[doc(inline)]
pub use __enq__ as enq;

/// Enqueue 1 or an optionally provided amount of items to the given queue group
#[doc(hidden)]
#[macro_export]
macro_rules! __enq_group__ {
    ($queue_group:expr, id: $distinguisher:expr) => {{
        $crate::queue::enq_group!($queue_group, id: $distinguisher, 1);
    }};
    ($queue_group:expr, id: $distinguisher:expr, $nenq:expr) => {{
        const queue_group: $crate::queue::QueueGroup = $queue_group;
        $crate::queue::tracing::span!(
            target: $crate::queue::TARGET,
            queue_group.0.level,
            queue_group.0.name,
        ).in_scope(|| {
            $crate::queue::tracing::event!(
                target: $crate::queue::TARGET,
                queue_group.0.level,
                { $crate::queue::NENQ_FIELD_ID } = $nenq as u64,
                { $crate::queue::DISTINGUISHER_FIELD_ID } = $distinguisher as u64,
            );
        });
    }};
}
#[doc(inline)]
pub use __enq_group__ as enq_group;

/// Dequeue 1 or an optionally provided amount of items to the given queue
#[doc(hidden)]
#[macro_export]
macro_rules! __deq__ {
    ($queue:expr) => {{
        $crate::queue::deq!($queue, 1);
    }};
    ($queue:expr, $ndeq:expr) => {{
        const queue: $crate::queue::Queue = $queue;
        $crate::queue::tracing::span!(
            target: $crate::queue::TARGET,
            queue.0.level,
            queue.0.name,
        ).in_scope(|| {
            $crate::queue::tracing::event!(
                target: $crate::queue::TARGET,
                queue.0.level,
                { $crate::queue::NDEQ_FIELD_ID } = $ndeq as u64,
            );
        });
    }};
}
#[doc(inline)]
pub use __deq__ as deq;

/// Dequeue 1 or an optionally provided amount of items to the given queue group
#[doc(hidden)]
#[macro_export]
macro_rules! __deq_group__ {
    ($queue_group:expr, id: $distinguisher:expr) => {{
        $crate::queue::deq_group!($queue_group, id: $distinguisher, 1);
    }};
    ($queue_group:expr, id: $distinguisher:expr, $ndeq:expr) => {{
        const queue_group: $crate::queue::QueueGroup = $queue_group;
        $crate::queue::tracing::span!(
            target: $crate::queue::TARGET,
            queue_group.0.level,
            queue_group.0.name,
        ).in_scope(|| {
            $crate::queue::tracing::event!(
                target: $crate::queue::TARGET,
                queue_group.0.level,
                { $crate::queue::NDEQ_FIELD_ID } = $ndeq as u64,
                { $crate::queue::DISTINGUISHER_FIELD_ID } = $distinguisher as u64,
            );
        });
    }};
}
#[doc(inline)]
pub use __deq_group__ as deq_group;

struct QueueLayer {
    writer_thread: Option<JoinHandle<()>>,
    event_tx: mpsc::Sender<EventNotification>,
}

impl QueueLayer {
    fn new<P: AsRef<Path>>(output_directory: P) -> Result<Self> {
        let output_directory = PathBuf::from(output_directory.as_ref());
        let (event_tx, event_rx) = mpsc::channel();

        if output_directory.exists() && !output_directory.is_dir() {
            anyhow::bail!("{:?} is not a directory", output_directory);
        }
        fs::create_dir_all(&output_directory)?;

        let writer_thread = Some(std::thread::spawn(move || {
            let mut queues = BTreeMap::new();
            let mut queue_groups = BTreeMap::new();

            event_rx.iter().for_each(|notification: EventNotification| {
                let history = match notification.distinguisher_id {
                    Some(id) => {
                        let map = queue_groups
                            .entry(notification.queue_name)
                            .or_insert(BTreeMap::new());
                        map.entry(id).or_insert_with_key(|id| {
                            let name = format!("{}-{}", notification.queue_name, id);
                            History::new(&output_directory, &name).unwrap_or_else(|e| {
                                panic!("create history for queue group '{}-{}': {}", name, id, e)
                            })
                        });
                        map.get_mut(&id).unwrap()
                    }
                    None => queues
                        .entry(notification.queue_name)
                        .or_insert_with_key(|name| {
                            History::new(&output_directory, name).unwrap_or_else(|e| {
                                panic!("create history for queue '{}': {}", name, e)
                            })
                        }),
                };
                history.update(notification).expect("update history");
            });
        }));

        Ok(QueueLayer {
            writer_thread,
            event_tx,
        })
    }
}

impl Drop for QueueLayer {
    fn drop(&mut self) {
        if let Some(handle) = self.writer_thread.take() {
            handle.join().expect("joining writer thread");
        }
    }
}

impl<S: Subscriber + for<'span> LookupSpan<'span>> Layer<S> for QueueLayer {
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        if let Some(span) = ctx.lookup_current() {
            let queue_name = span.name();
            let mut notification = EventNotification::new(queue_name);
            event.record(&mut notification);
            self.event_tx
                .send(notification)
                .expect("writer thread hung up");
        }
    }
}

#[derive(Debug)]
struct EventNotification {
    queue_name: &'static str,
    nenq: u64,
    ndeq: u64,
    distinguisher_id: Option<u64>,
    timestamp: Instant,
}

impl EventNotification {
    fn new(queue_name: &'static str) -> Self {
        EventNotification {
            queue_name,
            nenq: 0,
            ndeq: 0,
            distinguisher_id: None,
            timestamp: Instant::now(),
        }
    }
}

impl Visit for EventNotification {
    fn record_debug(&mut self, _field: &Field, _value: &dyn Debug) {
        // Intentionally do nothing
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        match field.name() {
            NENQ_FIELD_ID => self.nenq = value,
            NDEQ_FIELD_ID => self.ndeq = value,
            DISTINGUISHER_FIELD_ID => self.distinguisher_id = Some(value),
            _ => (),
        }
    }
}

#[derive(Debug)]
struct History {
    start_time: Instant,
    most_recent: Option<State>,
    writer: csv::Writer<File>,
}

impl History {
    fn new<P: AsRef<Path>>(output_directory: P, queue_name: &str) -> Result<Self> {
        let file_path = output_directory
            .as_ref()
            .join(queue_name)
            .with_extension("csv");
        let writer = csv::WriterBuilder::new()
            .has_headers(false)
            .from_path(file_path)?;
        Ok(History {
            start_time: Instant::now(),
            most_recent: None,
            writer,
        })
    }

    fn update(&mut self, notification: EventNotification) -> Result<()> {
        let new_state = match self.most_recent.take() {
            None => State {
                total_enq: notification.nenq,
                total_deq: notification.ndeq,
                timestamp: notification.timestamp - self.start_time,
            },
            Some(last) => {
                let mut new = last.clone();
                new.total_enq += notification.nenq;
                new.total_deq += notification.ndeq;
                new.timestamp = notification.timestamp - self.start_time;
                new
            }
        };
        self.writer.write_record([
            format!("{}", new_state.timestamp.as_nanos()),
            format!("{}", new_state.total_enq - new_state.total_deq),
        ])?;
        self.writer.flush()?;
        self.most_recent = Some(new_state);
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct State {
    total_enq: u64,
    total_deq: u64,
    timestamp: Duration,
}

pub(crate) fn layer<'a, S>(common_config: &crate::CommonConfiguration<'a>) -> Result<impl Layer<S>>
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    let queue_env_filter = EnvFilter::builder().parse(common_config.trace_filter)?;
    let output_directory = common_config.output_directory.join(TARGET);
    fs::create_dir_all(&output_directory)?;

    Ok(QueueLayer::new(output_directory)?
        .with_filter(queue_env_filter)
        .with_filter(filter_fn(|md| md.target() == TARGET)))
}
