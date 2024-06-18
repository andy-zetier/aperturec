//!
//! Metric exporters
//!
//! This module defines all available metric Exporters. Exporters are used to send Metric
//! mesurements to various destinations during each polling period. Exporters can be selectively
//! enabled via the [`with_exporter()`](crate::MetricsInitializer::with_exporter) method.
//!
use crate::Measurement;

use anyhow::{anyhow, Result};
use aperturec_trace::{log, Level};
use chrono::{SecondsFormat, Utc};
use prometheus::{Encoder, Gauge, Opts, TextEncoder};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::net::{Ipv4Addr, TcpStream};
use std::str::FromStr;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime};
use sysinfo::{ProcessExt, System, SystemExt, UserExt};
use tiny_http::{Response, Server, StatusCode};

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
    Prometheus(PrometheusExporter),
}

impl Exporter {
    pub fn export(&mut self, measurements: &[Measurement]) -> Result<()> {
        match self {
            Exporter::Log(e) => e.do_export(measurements),
            Exporter::Csv(e) => e.do_export(measurements),
            Exporter::Pushgateway(e) => e.do_export(measurements),
            Exporter::Prometheus(e) => e.do_export(measurements),
        }
    }
}

///
/// Writes metric data to the initialized log at the specified [`Level`]
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
            .append(true)
            .open(path)
            .map_err(|e| {
                log::error!("Failed to open metrics file '{}' for writing: {}", path, e);
                e
            })?)
    }
}

#[derive(Default)]
struct PrometheusProxy {
    gauges: HashMap<String, Gauge>,
    registration_status: HashMap<String, bool>,
}

impl PrometheusProxy {
    fn new() -> Self {
        Self {
            gauges: HashMap::new(),
            registration_status: HashMap::new(),
        }
    }

    fn update(&mut self, results: &[Measurement]) -> bool {
        if self.gauges.is_empty() {
            self.setup_gauges(results)
        }

        //
        // If any of the Measurements backed by Prometheus Gauges have changed in their validity
        // (ie. r.value.is_none() -> r.value.is_some()), we need to adjust the gauges we have
        // registered with the registry. There doesn't seem to be a way to see if a gauge is
        // currently registered, so we keep track of registration_status ourselves.
        //
        let mut updated_registrations = false;
        results.iter().for_each(|r| {
            let g = self.gauges.get_mut(&r.title);

            if let Some(g) = g {
                if let Some(r_value) = r.value {
                    g.set(r_value);
                }

                let is_registered = *self.registration_status.get(&r.title).unwrap();

                if r.value.is_some() != is_registered {
                    if !is_registered {
                        prometheus::default_registry()
                            .register(Box::new(g.clone()))
                            .unwrap();
                    } else {
                        prometheus::default_registry()
                            .unregister(Box::new(g.clone()))
                            .unwrap();
                    }

                    self.registration_status
                        .insert(r.title.to_string(), !is_registered);
                    updated_registrations = true;
                }
            }
        });

        updated_registrations
    }

    fn setup_gauges(&mut self, ers: &[Measurement]) {
        ers.iter().for_each(|e| {
            let gauge_opts = Opts::new(e.title_as_namespaced(), e.help.to_string());
            let gauge = Gauge::with_opts(gauge_opts).expect("Gauge Create");

            //
            // This may fail if the Measurement has already been registered with the
            // default registry. This is expected for macro-generated histogram metrics and allows
            // native prometheus metrics to be used along with those managed by this crate.
            //
            if prometheus::default_registry()
                .register(Box::new(gauge.clone()))
                .is_ok()
            {
                self.gauges.insert(e.title.to_string(), gauge);
                self.registration_status.insert(e.title.to_string(), true);
            }
        });
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
    groupings: HashMap<String, String>,
    needs_cleared: bool,
    prom_proxy: PrometheusProxy,
}

impl PushgatewayExporter {
    pub fn new(url: String, job_name: String, id: u32) -> Result<Self> {
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
            groupings,
            needs_cleared: false,
            prom_proxy: PrometheusProxy::new(),
        })
    }

    fn do_export(&mut self, results: &[Measurement]) -> Result<()> {
        //
        // If any of the Measurements have changed in their validity (ie. r.value.is_none() ->
        // r.value.is_some()), we need to clear out the most recently pushed metrics in the
        // Pushgateway before uploading the adjusted set.
        //
        if self.prom_proxy.update(results) {
            self.clear_metrics()?;
        }

        prometheus::push_metrics(
            &self.job_name,
            self.groupings.clone(),
            &self.url,
            prometheus::default_registry().gather(),
            None,
        )?;

        self.needs_cleared = true;
        Ok(())
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

///
/// Hosts metric data for Prometheus
///
#[derive(Default)]
pub struct PrometheusExporter {
    prom_proxy: PrometheusProxy,
}

impl PrometheusExporter {
    pub fn new(bind_addr: &str) -> Result<Self> {
        const DEFAULT_PORT: u16 = 8080;
        const DEFAULT_ADDR: Ipv4Addr = Ipv4Addr::LOCALHOST;

        let bind_addr = match bind_addr.rsplit_once(':') {
            None => format!("{}:{}", bind_addr, DEFAULT_PORT),
            Some((a, p)) => {
                if a.is_empty() {
                    format!("{}:{}", DEFAULT_ADDR, p)
                } else if p.parse::<u16>().is_err() {
                    // Presume ipv6 bind address with no port specified
                    format!("{}:{}", bind_addr, DEFAULT_PORT)
                } else {
                    bind_addr.to_string()
                }
            }
        };

        let server = Arc::new(Server::http(bind_addr).map_err(|e| anyhow!(e))?);
        log::debug!(
            "Prometheus metrics webserver bound to {}",
            server.server_addr()
        );

        thread::spawn(move || {
            const METRICS_SLUG: &str = "/metrics";
            let encoder = TextEncoder::new();
            let header =
                tiny_http::Header::from_str("Content-Type: text/plain; version=0.0.4").unwrap();

            loop {
                match server.recv() {
                    Ok(request) => {
                        let response = match request.url() {
                            METRICS_SLUG => {
                                let mut buffer = vec![];
                                match encoder
                                    .encode(&prometheus::default_registry().gather(), &mut buffer)
                                {
                                    Ok(()) => Response::from_data(buffer)
                                        .with_header(header.clone())
                                        .boxed(),
                                    Err(e) => {
                                        log::warn!("Failed to encode Prometheus metrics: {}", e);
                                        Response::empty(StatusCode(500)).boxed()
                                    }
                                }
                            }
                            _ => Response::empty(StatusCode(404)).boxed(),
                        };

                        if let Err(e) = request.respond(response) {
                            log::warn!("Failed to respond to Prometheus request: {}", e);
                        }
                    }
                    Err(err) => {
                        log::error!("Prometheus metrics I/O error: {:?}", err);
                        break;
                    }
                }
            }
            log::debug!("Prometheus metrics webserver shutting down");
        });

        Ok(Self {
            prom_proxy: PrometheusProxy::new(),
        })
    }

    fn do_export(&mut self, results: &[Measurement]) -> Result<()> {
        self.prom_proxy.update(results);
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::measurement::Measurement;
    use std::io::Read;
    use std::sync::atomic::{AtomicBool, Ordering};
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
        let server = Arc::new(Server::http("127.0.0.1:0").unwrap());
        let is_running = Arc::new(AtomicBool::new(true));

        let _jh = {
            let server = server.clone();
            let is_running = is_running.clone();
            let put_request = put_request.clone();
            let delete_request = delete_request.clone();
            thread::spawn(move || {
                while is_running.load(Ordering::SeqCst) {
                    if let Ok(Some(request)) = server.recv_timeout(Duration::from_millis(100)) {
                        let response = match request.method().as_str() {
                            "PUT" => {
                                let mut s = put_request.lock().unwrap();
                                *s = format!("PUT {}", request.url());
                                Response::from_string("Ok").boxed()
                            }
                            "DELETE" => {
                                let mut s = delete_request.lock().unwrap();
                                *s = format!("DELETE {}", request.url());
                                Response::from_string("Ok").boxed()
                            }
                            _ => Response::empty(StatusCode(404)).boxed(),
                        };

                        request.respond(response).unwrap();
                    }
                }
            })
        };

        let port = server.server_addr().to_ip().unwrap().port();
        let id = std::process::id();

        let mut pge =
            PushgatewayExporter::new(format!("http://127.0.0.1:{}", port), JOB.to_string(), id)
                .expect("Pushgateway Exporter");

        // Generate a PUSH request
        pge.do_export(&generate_measurements()).expect("export");

        // Generate a DROP request
        drop(pge);

        // shutdown fake pushgateway
        is_running.store(false, Ordering::SeqCst);

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

    #[test]
    fn prometheus_exporter() {
        const URL: &str = "127.0.0.1:8080";

        let mut pe = PrometheusExporter::new(URL).expect("PrometheusExporter");
        pe.do_export(&generate_measurements()).expect("export");

        let response = Client::builder()
            .build()
            .unwrap()
            .request(
                Method::GET,
                Url::from_str(&format!("http://{}/bad_endpoint", URL)).unwrap(),
            )
            .send();

        let response = response.unwrap();
        assert_eq!(response.status().as_str(), "404");

        let response = Client::builder()
            .build()
            .unwrap()
            .request(
                Method::GET,
                Url::from_str(&format!("http://{}/metrics", URL)).unwrap(),
            )
            .send();

        let response = response.unwrap();
        let status = response.status();
        let text = response.text().unwrap();

        assert_eq!(status.as_str(), "200");
        assert!(
            text.contains(
                "# HELP ac_bars All bars available\n# TYPE ac_bars gauge\nac_bars 33.33333"
            ),
            "\"{:?}\" ⊉ \"# HELP ac_bars ...\"",
            text
        );
    }
}
