//!
//! Metric exporters
//!
//! This module defines all available metric Exporters. Exporters are used to send Metric
//! mesurements to various destinations during each polling period. Exporters can be selectively
//! enabled via the [`with_exporter()`](crate::MetricsInitializer::with_exporter) method.
//!
use crate::Measurement;

use anyhow::{anyhow, Result};
use chrono::{SecondsFormat, Utc};
use prometheus::{Encoder, Gauge, Opts, TextEncoder};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::net::{Ipv4Addr, TcpStream};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime};
use tiny_http::{Response, Server, StatusCode};
use tracing::*;

use reqwest::blocking::Client;
use reqwest::{Method, Url};

///
/// All available exporters
///
/// These can be selected individually using the
/// [`with_exporter()`](crate::MetricsInitializer::with_exporter) method.
///

#[derive(Debug)]
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

    pub fn register_measurements<'m>(
        &mut self,
        measurements: impl IntoIterator<Item = &'m Measurement>,
    ) -> Result<()> {
        match self {
            Exporter::Csv(e) => e.register_measurements(measurements),
            Exporter::Pushgateway(e) => e.register_measurements(measurements),
            Exporter::Prometheus(e) => e.register_measurements(measurements),
            _ => Ok(()),
        }
    }
}

///
/// Writes metric data to the initialized log at the specified [`Level`]
///
#[derive(Debug)]
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
            Level::ERROR => error!("{}", s),
            Level::WARN => warn!("{}", s),
            Level::INFO => info!("{}", s),
            Level::DEBUG => debug!("{}", s),
            Level::TRACE => trace!("{}", s),
        };
        Ok(())
    }
}

///
/// Writes metric data to a CSV file
///
/// If `path` does not exist, a header line will be written before appending metric data. If `path`
/// exists, a new file will be created with a .N extension where N is the first available integer.
/// Calls to [`new()`](CsvExporter::new) may fail if `path` cannot be opened.
///
#[derive(Debug)]
pub struct CsvExporter {
    file: Option<std::fs::File>,
    path: String,
    header: String,
}

impl CsvExporter {
    pub fn new(path: String) -> Result<Self> {
        Ok(Self {
            file: None,
            path,
            header: "".to_string(),
        })
    }

    fn register_measurements<'m>(
        &mut self,
        measurements: impl IntoIterator<Item = &'m Measurement>,
    ) -> Result<()> {
        let is_header_missing = measurements
            .into_iter()
            .any(|m| !self.header.contains(&Self::generate_title(m)));

        if is_header_missing {
            self.file.take();
        }
        Ok(())
    }

    fn generate_title(m: &Measurement) -> String {
        if m.units.is_empty() {
            m.title_as_lower()
        } else {
            format!("{}_{}", m.title_as_lower(), m.units_as_lower())
        }
    }

    fn generate_header(results: &[Measurement]) -> String {
        let mut header = Vec::with_capacity(1 + results.len());
        header.push("timestamp_rfc3339".to_string());
        header.extend(results.iter().map(Self::generate_title));
        header.join(",")
    }

    fn update_header(&mut self, results: &[Measurement]) {
        self.header = Self::generate_header(results);
    }

    fn do_export(&mut self, results: &[Measurement]) -> Result<()> {
        if self.file.is_none() {
            self.update_header(results);
            self.reopen_file()?;
            writeln!(self.file.as_ref().unwrap(), "{}", self.header)
                .or_else(|_| self.reopen_file())?;
        }

        let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let line = results
            .iter()
            .map(|r| {
                if r.value.is_some() {
                    format!("{:.6?}", r.value.unwrap())
                } else {
                    "".to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(",");
        writeln!(self.file.as_ref().unwrap(), "{},{}", timestamp, line).or_else(|e| {
            warn!("Failed to write metrics to '{}': {}", self.path, e);
            self.reopen_file()
        })?;
        Ok(())
    }

    fn reopen_file(&mut self) -> Result<()> {
        self.file = Some(Self::open_file(&self.path)?);
        Ok(())
    }

    fn open_file(path_str: &str) -> Result<std::fs::File> {
        let path = Path::new(path_str);

        if let Ok(f) = OpenOptions::new().write(true).open(path) {
            if f.metadata()?.len() == 0 {
                return Ok(f);
            }
        }

        let mut last_error = None;
        for count in 0..1000 {
            let new_path = path.with_file_name(format!(
                "{}.{}{}",
                path.file_stem().unwrap_or_default().to_string_lossy(),
                count,
                path.extension()
                    .map(|ext| format!(".{}", ext.to_string_lossy()))
                    .unwrap_or_default()
            ));

            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&new_path)
            {
                Ok(f) => return Ok(f),
                Err(e) => match e.kind() {
                    std::io::ErrorKind::AlreadyExists => last_error = Some(e),
                    _ => {
                        error!(
                            "Failed to open metrics file '{}' for writing: {:?}",
                            new_path.display(),
                            e
                        );
                        return Err(e.into());
                    }
                },
            }
        }

        Err(last_error
            .unwrap_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::Other, "Unknown error occurred")
            })
            .into())
    }
}

#[derive(Debug, Default)]
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
        //
        // If any of the Measurements backed by Prometheus Gauges have changed in their validity
        // (ie. r.value.is_none() -> r.value.is_some()), we need to adjust the gauges we have
        // registered with the registry. There doesn't seem to be a way to see if a gauge is
        // currently registered, so we keep track of registration_status ourselves. Note, Histogram
        // Measurements are not backed by Gauges and are ignored.
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

    fn register_measurements<'m>(
        &mut self,
        measurements: impl IntoIterator<Item = &'m Measurement>,
    ) -> Result<()> {
        measurements.into_iter().for_each(|m| {
            let gauge_opts = Opts::new(m.title_as_namespaced(), m.help.to_string());
            let gauge = Gauge::with_opts(gauge_opts).expect("Gauge Create");

            //
            // Measurements may have already been registered with the Prometheus registry. This is
            // expected for macro-generated histogram metrics and allows native prometheus metrics
            // to be used along with those managed by this crate.
            //
            match prometheus::default_registry().register(Box::new(gauge.clone())) {
                Ok(()) => {
                    self.gauges.insert(m.title.to_string(), gauge);
                    self.registration_status.insert(m.title.to_string(), true);
                }
                Err(prometheus::Error::AlreadyReg) => {
                    trace!(
                        "Prometheus Collector '{}' already registered, ignoring",
                        m.title
                    )
                }
                Err(e) => {
                    error!("Prometheus gauge '{}' failed to register: {:?}", m.title, e);
                }
            };
        });
        Ok(())
    }
}

///
/// Sends metric data to a Prometheus Pushgateway
///
/// Calls to [`new()`](PushgatewayExporter::new) will fail if URL is unresolvable. Connection
/// errors will warn. Metric data pushed to a Pushgateway may become stale and continue to
/// persist even after the process shuts down due to loss of network connectivity or unexpected
/// process termination. Stale metric data can be deleted from the Pushgateway using the web UI or
/// the available `pg_clean.sh` included in the source tree. See the [Prometheus
/// Pushgateway](https://github.com/prometheus/pushgateway#prometheus-pushgateway) docs for more
/// info.
///
#[derive(Debug, Default)]
pub struct PushgatewayExporter {
    url: String,
    job_name: String,
    groupings: HashMap<String, String>,
    needs_cleared: bool,
    prom_proxy: PrometheusProxy,
}

impl PushgatewayExporter {
    pub fn new(url: String, job_name: String, id: u32) -> Result<Self> {
        // Ensure Pushgateway URL is resolvable, warn on failed connection
        {
            if let Err(e) = TcpStream::connect(Url::parse(&url)?.socket_addrs(|| None)?.as_slice())
            {
                warn!("Pushgateway Exporter failed to connect to {}: {}", url, e);
            }
        }

        let mut groupings = HashMap::new();
        groupings.insert("id".to_owned(), id.to_string());
        groupings.insert("instance".to_owned(), "".to_string());
        groupings.insert("user".to_owned(), whoami::username());

        if let Ok(epoch) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
            groupings.insert("instance".to_owned(), epoch.as_secs().to_string());
        }

        if let Ok(host_name) = whoami::fallible::hostname() {
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

    fn register_measurements<'m>(
        &mut self,
        results: impl IntoIterator<Item = &'m Measurement>,
    ) -> Result<()> {
        self.prom_proxy.register_measurements(results)
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

    fn clear_metrics(&mut self) -> Result<()> {
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
        self.needs_cleared = true;
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
#[derive(Debug, Default)]
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
        debug!(
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
                                        warn!("Failed to encode Prometheus metrics: {}", e);
                                        Response::empty(StatusCode(500)).boxed()
                                    }
                                }
                            }
                            _ => Response::empty(StatusCode(404)).boxed(),
                        };

                        if let Err(e) = request.respond(response) {
                            warn!("Failed to respond to Prometheus request: {}", e);
                        }
                    }
                    Err(err) => {
                        error!("Prometheus metrics I/O error: {:?}", err);
                        break;
                    }
                }
            }
            debug!("Prometheus metrics webserver shutting down");
        });

        Ok(Self {
            prom_proxy: PrometheusProxy::new(),
        })
    }

    fn register_measurements<'m>(
        &mut self,
        results: impl IntoIterator<Item = &'m Measurement>,
    ) -> Result<()> {
        self.prom_proxy.register_measurements(results)
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
    use std::fs::{self, File};
    use std::io::Read;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use tempfile::NamedTempFile;
    use test_log::test;

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

    struct TestWriter(Mutex<Vec<u8>>);

    impl Write for &TestWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.0.lock().expect("lock").flush()
        }
    }

    #[test]
    fn log_exporter() {
        let log_writer = Arc::new(TestWriter(Mutex::new(vec![])));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(log_writer.clone())
            .with_env_filter(
                tracing_subscriber::EnvFilter::builder()
                    .with_default_directive(Level::DEBUG.into())
                    .from_env()
                    .unwrap(),
            )
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let mut le = LogExporter::new(Level::DEBUG).expect("LogExporter");
            le.do_export(&generate_measurements()).expect("export");
        });

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
        let path = file.path().to_str().expect("path").to_string();
        let mut measurements = generate_measurements();

        let mut ce = CsvExporter::new(path).expect("CsvExporter");

        ce.register_measurements(&measurements).expect("register");
        ce.do_export(&measurements).expect("export");

        let mut contents = String::new();
        file.read_to_string(&mut contents).expect("read");

        // Expect 2 lines in CSV file: 1 header line, 1 value line
        assert_eq!(contents.lines().collect::<Vec<_>>().len(), 2);

        // Append an additional value line
        ce.do_export(&measurements).expect("export");
        file.read_to_string(&mut contents).expect("read");
        let lines: Vec<&str> = contents.lines().collect();

        // Expect 3 lines in CSV file: 1 header line, 2 value lines
        assert_eq!(lines.len(), 3);

        // Expect one comma for each field
        assert_eq!(measurements.len(), lines[0].matches(',').count());

        // Expect the same number of fields in the header line as in the data lines
        assert_eq!(lines[0].matches(',').count(), lines[1].matches(',').count());
        assert_eq!(lines[0].matches(',').count(), lines[2].matches(',').count());

        //
        // Test registering a new metric after the first do_export()
        //

        let mut new_measurements = vec![Measurement::new(
            "Baz",
            Some(6.0),
            "Luhrmann",
            "Movies directed",
        )];

        ce.register_measurements(&new_measurements)
            .expect("register");
        measurements.append(&mut new_measurements);
        ce.do_export(&measurements).expect("export");

        let expected_path = file.path().with_file_name(format!(
            "{}.0",
            file.path()
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
        ));

        let mut csv_file = File::open(expected_path.clone()).expect("open next file");
        contents.clear();
        csv_file.read_to_string(&mut contents).expect("read");
        let lines: Vec<&str> = contents.lines().collect();

        // Expect 2 lines in CSV file: 1 header line, 1 value line
        assert_eq!(lines.len(), 2);

        // Expect one comma for each field
        assert_eq!(measurements.len(), lines[0].matches(',').count());

        // Expect the same number of fields in the header line as in the data line
        assert_eq!(lines[0].matches(',').count(), lines[1].matches(',').count());

        drop(file);
        fs::remove_file(expected_path).expect("remove temp file");
    }

    #[test]
    fn pushgateway_exporter() {
        const JOB: &str = "pushgateway_unit_test";

        // Pushgateway exporter can be initalized before server is running
        {
            let _pge = PushgatewayExporter::new(
                format!("http://127.0.0.1:{}", 1234),
                JOB.to_string(),
                5678,
            )
            .expect("Pushgateway Exporter");
        }

        let put_request: Arc<Mutex<String>> = Arc::new(Mutex::from(String::new()));
        let put_content: Arc<Mutex<String>> = Arc::new(Mutex::from(String::new()));
        let delete_request: Arc<Mutex<String>> = Arc::new(Mutex::from(String::new()));
        let server = Arc::new(Server::http("127.0.0.1:0").unwrap());
        let is_running = Arc::new(AtomicBool::new(true));

        let _jh = {
            let server = server.clone();
            let is_running = is_running.clone();
            let put_request = put_request.clone();
            let put_content = put_content.clone();
            let delete_request = delete_request.clone();
            thread::spawn(move || {
                while is_running.load(Ordering::SeqCst) {
                    if let Ok(Some(mut request)) = server.recv_timeout(Duration::from_millis(100)) {
                        let response = match request.method().as_str() {
                            "PUT" => {
                                let mut s = put_request.lock().unwrap();
                                *s = format!("PUT {}", request.url());
                                let mut content = Vec::new();
                                request.as_reader().read_to_end(&mut content).unwrap();
                                let mut c = put_content.lock().unwrap();
                                *c = String::from_utf8_lossy(&content).to_string();
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
        let measurements = generate_measurements();

        let mut pge =
            PushgatewayExporter::new(format!("http://127.0.0.1:{}", port), JOB.to_string(), id)
                .expect("Pushgateway Exporter");

        pge.register_measurements(&measurements).expect("register");

        // Generate a PUSH request
        pge.do_export(&measurements).expect("export");

        // Generate a DROP request
        drop(pge);

        // Shutdown fake pushgateway
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

        // Content is protobuf encoded, but the title and help strings should be in plaintext
        let content = put_content.lock().unwrap();
        for s in measurements
            .into_iter()
            .map(|m| vec![m.title_as_namespaced(), m.help])
            .collect::<Vec<_>>()
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
        {
            assert!(content.contains(&s), "\"{:?}\" ⊉ \"{:?}\"", content, s);
        }
    }

    #[test]
    fn prometheus_exporter() {
        const URL: &str = "127.0.0.1:8946";
        let measurements = generate_measurements();

        let mut pe = PrometheusExporter::new(URL).expect("PrometheusExporter");

        pe.register_measurements(&measurements).expect("register");
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
            "\"{:?}\" ⊉ \"# HELP ac_bars ALL bars ...\"",
            text
        );
    }
}
