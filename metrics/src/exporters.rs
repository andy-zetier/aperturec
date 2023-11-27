//!
//! Metric exporters
//!
//! This module defines all available metric Exporters. Exporters are used to send Metric
//! mesurements to various destinations during each polling period. Exporters can be selecteively
//! enabled via the [`with_exporter()`](crate::MetricsInitializer::with_exporter) method.
//!
use crate::Measurement;

use anyhow::Result;
use aperturec_trace::{log, Level};
use chrono::{SecondsFormat, Utc};
use prometheus::{Gauge, Opts, Registry};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::net::TcpStream;
use std::str::FromStr;
use std::time::{Duration, SystemTime};
use sysinfo::{ProcessExt, System, SystemExt, UserExt};

use reqwest::blocking::Client;
use reqwest::{Method, Url};

///
/// All available exporters
///
/// These can be selected individually using the
/// [`with_exporter()`](crate::MetricsInitializer::with_exporter) method.
///

pub enum Exporter {
    Log(LogExporter),
    Csv(CsvExporter),
    Pushgateway(PushgatewayExporter),
}

impl Exporter {
    pub fn export(&mut self, measurements: &[Measurement]) -> Result<()> {
        match self {
            Exporter::Log(e) => e.do_export(measurements),
            Exporter::Csv(e) => e.do_export(measurements),
            Exporter::Pushgateway(e) => e.do_export(measurements),
        }
    }
}

///
/// Writes metric data to the initialized log at the spcified [`Level`]
///
pub struct LogExporter {
    log_level: Level,
}

impl LogExporter {
    pub fn new(log_level: Level) -> Result<Self> {
        Ok(Self { log_level })
    }

    fn do_export(&mut self, results: &[Measurement]) -> Result<()> {
        let s = results
            .iter()
            .map(|r| format!("[{}]", r))
            .collect::<Vec<_>>()
            .join("");
        match self.log_level {
            Level::ERROR => log::error!("{}", s),
            Level::WARN => log::warn!("{}", s),
            Level::INFO => log::info!("{}", s),
            Level::DEBUG => log::debug!("{}", s),
            Level::TRACE => log::trace!("{}", s),
        };
        Ok(())
    }
}

///
/// Writes metric data to a CSV file
///
/// If the file specified by `path` exists, metric data will be appended to it. If `path` does not
/// exist, a header line will be written before appending metric data. Calls to
/// [`new()`](CsvExporter::new) may fail if `path` cannot be opened.
///
pub struct CsvExporter {
    pub path: String,
    file: Option<std::fs::File>,
    needs_header: bool,
}

impl CsvExporter {
    pub fn new(path: String) -> Result<Self> {
        let file = Some(Self::open_file(&path)?);
        let needs_header = file.as_ref().unwrap().metadata().unwrap().len() == 0;

        Ok(Self {
            path,
            file,
            needs_header,
        })
    }

    fn do_export(&mut self, results: &[Measurement]) -> Result<()> {
        if self.needs_header {
            let header = results
                .iter()
                .map(|r| format!("{}_{}", r.title_as_lower(), r.units_as_lower(),))
                .collect::<Vec<_>>()
                .join(",");
            writeln!(self.file.as_ref().unwrap(), "timestamp_rfc3339,{}", header).or_else(|e| {
                log::warn!("Failed to write metrics header to '{}': {}", self.path, e);
                self.reopen_file()
            })?;
            self.needs_header = false;
        }

        let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let line = results
            .iter()
            .map(|r| {
                if r.value.is_some() {
                    format!("{:.6?}", r.value.unwrap())
                } else {
                    "--------".to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(",");
        writeln!(self.file.as_ref().unwrap(), "{},{}", timestamp, line).or_else(|e| {
            log::warn!("Failed to write metrics to '{}': {}", self.path, e);
            self.reopen_file()
        })?;
        Ok(())
    }

    fn reopen_file(&mut self) -> Result<()> {
        self.file = Some(Self::open_file(&self.path)?);
        Ok(())
    }

    fn open_file(path: &str) -> Result<std::fs::File> {
        Ok(OpenOptions::new()
            .create(true)
            .write(true)
            .append(true)
            .open(path)
            .map_err(|e| {
                log::error!("Failed to open metrics file '{}' for writing: {}", path, e);
                e
            })?)
    }
}

///
/// Sends metric data to a Prometheus Pushgateway
///
/// Calls to [`new()`](PushgatewayExporter::new) may fail if there does not appear to be anything
/// listening at `url`. Metric data pushed to a Pushgateway may become stale and continue to
/// persist even after the process shuts down due to loss of network connectivity or unexpected
/// process termination. Stale metric data can be deleted from the Pushgateway using the web UI or
/// the available `pg_clean.sh` included in the source tree. See the [Prometheus
/// Pushgateway](https://github.com/prometheus/pushgateway#prometheus-pushgateway) docs for more
/// info.
///
#[derive(Default)]
pub struct PushgatewayExporter {
    url: String,
    job_name: String,
    gauges: HashMap<String, Gauge>,
    registry: Option<Registry>,
    groupings: HashMap<String, String>,
    registration_status: HashMap<String, bool>,
    needs_cleared: bool,
}

impl PushgatewayExporter {
    pub fn new(url: String, job_name: String, id: u16) -> Result<Self> {
        // Ensure we can connect to the Pushgateway
        drop(TcpStream::connect(
            Url::parse(&url)?.socket_addrs(|| None).unwrap().as_slice(),
        )?);

        let mut groupings = HashMap::new();
        groupings.insert("id".to_owned(), id.to_string());
        groupings.insert("instance".to_owned(), "".to_string());
        groupings.insert("user".to_owned(), "".to_string());

        let s = System::new_all();
        if let Some(process) = s.process(sysinfo::get_current_pid().unwrap()) {
            if let Some(uid) = process.user_id() {
                if let Some(user) = s.get_user_by_id(uid) {
                    groupings.insert("user".to_owned(), user.name().to_owned());
                }
            }
        }

        if let Ok(epoch) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
            groupings.insert("instance".to_owned(), epoch.as_secs().to_string());
        }

        if let Some(host_name) = s.host_name() {
            groupings.insert("id".to_owned(), format!("{}:{}", host_name, id));
        }

        Ok(Self {
            url,
            job_name,
            gauges: HashMap::new(),
            registry: None,
            groupings,
            registration_status: HashMap::new(),
            needs_cleared: false,
        })
    }

    fn do_export(&mut self, results: &[Measurement]) -> Result<()> {
        if self.registry.is_none() {
            self.setup_registry(results);
        }

        //
        // If any of the metrics have changed in their validity (ie. r.value.is_none() ->
        // r.value.is_some()), we need to adjust the gauges we have registered with the registry
        // and clear out the most recently pushed metrics in the Pushgateway before uploading the
        // adjusted set. There doesn't seem to be a way to see if a gauge is currently registered,
        // so we keep track of registration_status ourselves.
        //
        let mut clear_before_push = false;
        results.iter().for_each(|r| {
            let g = self.gauges.get_mut(&r.title).expect("Gauge Access");
            if r.value.is_none() {
                if *self.registration_status.get(&r.title).unwrap() {
                    self.registry
                        .as_mut()
                        .unwrap()
                        .unregister(Box::new(g.clone()))
                        .unwrap();
                    self.registration_status.insert(r.title.to_string(), false);
                    clear_before_push = true;
                }
            } else {
                g.set(r.value.unwrap());
                if !*self.registration_status.get(&r.title).unwrap() {
                    self.registry
                        .as_mut()
                        .unwrap()
                        .register(Box::new(g.clone()))
                        .unwrap();
                    self.registration_status.insert(r.title.to_string(), true);
                    clear_before_push = true;
                }
            }
        });

        if clear_before_push {
            self.clear_metrics()?;
        }

        prometheus::push_metrics(
            &self.job_name,
            self.groupings.clone(),
            &self.url,
            self.registry.as_mut().unwrap().gather(),
            None,
        )?;

        self.needs_cleared = true;
        Ok(())
    }

    //
    // Called once during the first do_export() to setup the Prometheus gauges and registry
    //
    fn setup_registry(&mut self, ers: &[Measurement]) {
        self.registry = Some(Registry::new());
        ers.iter().for_each(|e| {
            let gauge_opts = Opts::new(e.title_as_namespaced(), e.help.to_string());
            let gauge = Gauge::with_opts(gauge_opts).expect("Gauge Create");
            self.registry
                .as_mut()
                .unwrap()
                .register(Box::new(gauge.clone()))
                .unwrap();
            self.gauges.insert(e.title.to_string(), gauge);
            self.registration_status.insert(e.title.to_string(), true);
        });
    }

    fn clear_metrics(&self) -> Result<()> {
        let delete_url = format!(
            "{}/metrics/job/{}/{}",
            &self.url,
            &self.job_name,
            &self
                .groupings
                .iter()
                .map(|(k, v)| format!("{}/{}", k, v))
                .collect::<Vec<_>>()
                .join("/")
        );
        Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap()
            .request(Method::DELETE, Url::from_str(&delete_url).unwrap())
            .send()?;
        Ok(())
    }
}

impl Drop for PushgatewayExporter {
    fn drop(&mut self) {
        if self.needs_cleared {
            let _ = self.clear_metrics();
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::measurement::Measurement;
    use rouille::{Response, Server};
    use std::io::Read;
    use std::sync::{Arc, Mutex};
    use tempfile::NamedTempFile;

    fn generate_measurements() -> Vec<Measurement> {
        vec![
            Measurement::new(
                "Bean Counter",
                Some(123.45),
                "shipped cans",
                "Counts the total cans of beans in jiffies",
            ),
            Measurement::new("Foos", Some(2.0), "Furlongs", "Total Foos in furlongs"),
            Measurement::new("Bars", Some(33.33333), "bars", "All bars available"),
        ]
    }

    #[derive(Clone)]
    struct TestWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for TestWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.0.lock().expect("lock").flush()
        }
    }

    #[test]
    fn log_exporter() {
        let log_writer = TestWriter(Arc::default());
        aperturec_trace::Configuration::new("test")
            .cmdline_verbosity(aperturec_trace::Level::DEBUG)
            .writer(log_writer.clone())
            .simple(true)
            .initialize()
            .expect("log initialize");

        let mut le = LogExporter::new(aperturec_trace::Level::DEBUG).expect("LogExporter");
        le.do_export(&generate_measurements()).expect("export");

        assert!(
            String::from_utf8_lossy(&log_writer.0.lock().expect("lock")).contains(&format!(
                "[{}]",
                generate_measurements()
                    .iter()
                    .map(|m| m.to_string())
                    .collect::<Vec<_>>()
                    .join("][")
                    .as_str()
            ))
        );
    }

    #[test]
    fn csv_exporter() {
        let mut file = NamedTempFile::new().expect("tempfile");

        let mut ce =
            CsvExporter::new(file.path().to_str().expect("path").to_string()).expect("CsvExporter");
        ce.do_export(&generate_measurements()).expect("export");

        let mut contents = String::new();
        file.read_to_string(&mut contents).expect("read");

        // Expect 2 lines in CSV file: 1 header line, 1 value line
        assert_eq!(contents.lines().collect::<Vec<_>>().len(), 2);

        // Append an additional value line
        ce.do_export(&generate_measurements()).expect("export");
        file.read_to_string(&mut contents).expect("read");
        assert_eq!(contents.lines().collect::<Vec<_>>().len(), 3);

        // Expect 9 commas: 3 in header line, 3 in each value line
        assert_eq!(
            generate_measurements().len() * 3,
            contents
                .chars()
                .filter(|c| *c == ',')
                .collect::<Vec<_>>()
                .len()
        );
    }

    #[test]
    fn pushgateway_exporter() {
        const JOB: &str = "pushgateway_unit_test";

        let put_request: Arc<Mutex<String>> = Arc::new(Mutex::from(String::new()));
        let delete_request: Arc<Mutex<String>> = Arc::new(Mutex::from(String::new()));
        let pr_thread = put_request.clone();
        let dr_thread = delete_request.clone();

        let server = Server::new("127.0.0.1:0", move |request| match request.method() {
            "PUT" => {
                let mut s = pr_thread.lock().unwrap();
                *s = format!("PUT {}", request.url());
                Response::text("Ok")
            }
            "DELETE" => {
                let mut s = dr_thread.lock().unwrap();
                *s = format!("DELETE {}", request.url());
                Response::text("Ok")
            }
            _ => Response::empty_404(),
        })
        .expect("fake-pushgateway");

        let port = server.server_addr().port();
        let (jh, sender) = server.stoppable();

        let mut pge =
            PushgatewayExporter::new(format!("http://127.0.0.1:{}", port), JOB.to_string(), port)
                .expect("Pushgateway Exporter");

        // Generate a PUSH request
        pge.do_export(&generate_measurements()).expect("export");

        // Generate a DROP request
        drop(pge);

        // shutdown fake pushgateway
        let _ = sender.send(());
        jh.join().expect("join");

        let uri = format!("/metrics/job/{}/", JOB);
        vec!["PUT", "DELETE"]
            .iter()
            .zip(vec![put_request, delete_request])
            .for_each(|(m, s)| {
                let req = s.lock().expect("lock");
                let expected = format!("{} {}", m, uri);
                assert!(
                    req.starts_with(&expected),
                    "\"{:?}\"... != \"{}\"",
                    req.get(0..expected.len()),
                    expected
                );
            });
    }
}
