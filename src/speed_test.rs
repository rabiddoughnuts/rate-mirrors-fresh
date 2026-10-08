extern crate byte_unit;
extern crate reqwest;
use crate::config::{Config, default_client_builder};
use crate::countries::{Country, LinkTo, LinkType};
use crate::freshness;
use crate::mirror::Mirror;
use byte_unit::{Byte, UnitType};
use futures::future::join_all;
use itertools::Itertools;
use reqwest::{Client, Error as ReqwestError};
use std::cmp;
use std::collections::{HashMap, HashSet};
use std::convert::From;
use std::fmt;
use std::fmt::Debug;
use std::sync::mpsc::Sender;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use tokio::runtime::Runtime;
use tokio::sync::Semaphore;

pub struct SpeedTestResult {
    pub bytes_downloaded: usize,
    pub elapsed: Duration,
    pub speed: f64,
    pub connection_time: Duration,
    pub files_bytes_downloaded: usize,
    pub files_elapsed: Duration,
    pub files_connection_time: Duration,
    pub item: Mirror,
    pub freshness_quality: Option<f64>,
    pub latest_build_date: Option<i64>,
    pub freshness_packages: Option<freshness::PackageBuildDates>,
}
impl SpeedTestResult {
    pub fn new(
        item: Mirror,
        bytes_downloaded: usize,
        elapsed: Duration,
        connection_time: Duration,
    ) -> SpeedTestResult {
        SpeedTestResult {
            item,
            bytes_downloaded,
            elapsed,
            connection_time,
            files_bytes_downloaded: bytes_downloaded,
            files_elapsed: elapsed,
            files_connection_time: connection_time,
            speed: if elapsed.is_zero() {
                0.0
            } else {
                bytes_downloaded as f64 / elapsed.as_secs_f64()
            },
            freshness_quality: None,
            latest_build_date: None,
            freshness_packages: None,
        }
    }

    pub fn fmt_speed(&self) -> String {
        let speed = Byte::from_f64(self.speed).unwrap();
        format!("{:.1}/s", speed.get_appropriate_unit(UnitType::Decimal))
    }

    fn fmt_duration(d: &Duration) -> String {
        if d.as_secs() == 0 {
            format!("{}ms", d.as_millis())
        } else {
            format!("{:.2}s", d.as_secs_f64())
        }
    }

    pub fn fmt_elapsed(&self) -> String {
        Self::fmt_duration(&self.elapsed)
    }

    pub fn fmt_connection_time(&self) -> String {
        Self::fmt_duration(&self.connection_time)
    }

    fn use_files_measurement(&mut self) {
        self.bytes_downloaded = self.files_bytes_downloaded;
        self.elapsed = self.files_elapsed;
        self.connection_time = self.files_connection_time;
        self.speed = if self.elapsed.is_zero() {
            0.0
        } else {
            self.bytes_downloaded as f64 / self.elapsed.as_secs_f64()
        };
    }

    fn use_retest_measurement(&mut self, retest: &SpeedTestResult) {
        self.bytes_downloaded = retest.bytes_downloaded;
        self.elapsed = retest.elapsed;
        self.connection_time = retest.connection_time;
        self.speed = retest.speed;
    }
}

impl fmt::Debug for SpeedTestResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(country) = self.item.country {
            write!(f, "[{}] ", country.code)?;
        }
        let bytes = Byte::from_u128(self.bytes_downloaded as u128).unwrap();
        write!(
            f,
            "SpeedTestResult {{ speed: {}; downloaded: {}; elapsed: {}; connection_time: {} }}",
            self.fmt_speed(),
            bytes.get_appropriate_unit(UnitType::Decimal),
            self.fmt_elapsed(),
            self.fmt_connection_time(),
        )
    }
}

impl fmt::Display for SpeedTestResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} -> {}", self, self.item.url)
    }
}

pub type SpeedTestResults = Vec<SpeedTestResult>;

#[derive(Debug)]
pub enum SpeedTestError {
    ReqwestError(()),
    TooFewBytesDownloadedError,
}
impl From<ReqwestError> for SpeedTestError {
    fn from(_error: ReqwestError) -> Self {
        SpeedTestError::ReqwestError(())
    }
}

#[derive(Debug)]
enum RateStrategy {
    HubsFirst,
    DistanceFirst,
}

async fn test_single_mirror(
    mirror: Mirror,
    client: Client,
    reference: Option<Result<Arc<freshness::PackageBuildDates>, String>>,
    config: Arc<Config>,
    semaphore: Arc<Semaphore>,
    freshness_semaphore: Arc<Semaphore>,
    tx_progress: Sender<String>,
    check_freshness: bool,
) -> Result<SpeedTestResult, SpeedTestError> {
    let mut bytes_downloaded: usize = 0;

    let _permit = semaphore.acquire().await;
    tx_progress
        .send(format!("PROBING MIRROR {}", mirror.url))
        .unwrap();

    let started_connecting = Instant::now();
    let response = client
        .get(mirror.url_to_test.as_str())
        .timeout(Duration::from_millis(config.per_mirror_timeout))
        .send()
        .await;
    let mut response = match response {
        Ok(r) => r,
        Err(e) => {
            tx_progress
                .send(format!(
                    "{}FAILED TO CONNECT TO {}",
                    mirror
                        .country
                        .map(|c| format!("[{}] ", c.code))
                        .unwrap_or("".to_string())
                        .as_str(),
                    mirror.url_to_test.as_str(),
                ))
                .unwrap();
            return Err(e.into());
        }
    };
    let connection_time = started_connecting.elapsed();
    let started_ts = Instant::now();
    let mut prev_ts = started_ts;
    let mut speeds: Vec<f64> = Vec::with_capacity(config.eps_checks);
    let mut index = 0;
    let eps_checks_f64 = config.eps_checks as f64;
    let mut filling_up = true;
    let min_per_mirror_duration = Duration::from_millis(config.min_per_mirror);
    let max_per_mirror_duration = Duration::from_millis(config.max_per_mirror);

    let mut now = Instant::now();

    while let Ok(Ok(Some(chunk))) = tokio::time::timeout(
        {
            let total_download_time = now.duration_since(started_ts);
            if total_download_time >= max_per_mirror_duration {
                Duration::from_secs_f64(0.0)
            } else {
                max_per_mirror_duration - total_download_time
            }
        },
        response.chunk(),
    )
    .await
    {
        let chunk_size = chunk.len();
        bytes_downloaded += chunk_size;

        now = Instant::now();
        let chunk_speed = chunk_size as f64 / now.duration_since(prev_ts).as_secs_f64();
        prev_ts = now;

        if filling_up {
            speeds.push(chunk_speed);
            index = (index + 1) % config.eps_checks;
            if index == 0 {
                filling_up = false;
            }
        } else {
            speeds[index] = chunk_speed;
            index = (index + 1) % config.eps_checks;
        }
        let total_download_time = now.duration_since(started_ts);
        if bytes_downloaded >= config.min_bytes_per_mirror
            && total_download_time > min_per_mirror_duration
            && speeds.len() == config.eps_checks
        {
            let mean = speeds.iter().sum::<f64>() / eps_checks_f64;
            let variance = speeds
                .iter()
                .map(|speed| {
                    let diff = mean - *speed;
                    diff * diff
                })
                .sum::<f64>()
                / eps_checks_f64;
            let std_deviation = variance.sqrt();

            if std_deviation / mean <= config.eps || total_download_time >= max_per_mirror_duration
            {
                break;
            }
        }
    }
    drop(_permit);

    if bytes_downloaded < config.min_bytes_per_mirror {
        tx_progress
            .send(format!("TOO FEW BYTES LOADED {}", mirror.url.as_str()))
            .unwrap();
        return Err(SpeedTestError::TooFewBytesDownloadedError);
    }

    let mut speed_test_result = SpeedTestResult::new(
        mirror,
        bytes_downloaded,
        prev_ts.duration_since(started_ts),
        connection_time,
    );

    tx_progress.send(format!("{}", speed_test_result)).unwrap();

    if check_freshness {
        if let Some(db_url) = speed_test_result.item.database_url() {
            let check_result = match reference {
                Some(Ok(reference)) => {
                    let _permit = freshness_semaphore.acquire().await.unwrap();
                    freshness::check_mirror(client, db_url, reference, config.freshness_timeout)
                        .await
                }
                Some(Err(error)) => freshness::FreshnessCheckResult::reference_error(error),
                None => freshness::FreshnessCheckResult::reference_error(
                    "reference database path is unavailable".to_string(),
                ),
            };
            if let Some(error) = &check_result.error {
                tx_progress
                    .send(format!(
                        "    [WARN] {} freshness check failed: {}",
                        speed_test_result.item.url, error
                    ))
                    .unwrap();
            } else {
                let transfer = check_result.transfer.as_ref().unwrap();
                tx_progress
                    .send(format!(
                        "    {} local build age: {:+.4} days ({} of {} packages; DB {} bytes in {:.2}s)",
                        speed_test_result.item.url,
                        check_result.score,
                        check_result.packages_compared,
                        check_result.reference_packages,
                        transfer.bytes,
                        transfer.elapsed.as_secs_f64()
                    ))
                    .unwrap();
            }
            if check_result.packages.is_some() {
                if let Some(transfer) = &check_result.transfer {
                    include_db_transfer_in_speed(&mut speed_test_result, transfer);
                }
                speed_test_result.freshness_packages = check_result.packages;
            }
        }
    }

    Ok(speed_test_result)
}

fn test_mirrors<T: IntoIterator<Item = Mirror>>(
    mirrors: T,
    client: Client,
    references: &HashMap<String, Result<Arc<freshness::PackageBuildDates>, String>>,
    config: Arc<Config>,
    runtime: &Runtime,
    semaphore: Arc<Semaphore>,
    freshness_semaphore: Arc<Semaphore>,
    tx_progress: mpsc::Sender<String>,
    check_freshness: bool,
) -> SpeedTestResults {
    let mut handles = Vec::new();
    for mirror in mirrors.into_iter() {
        let reference = mirror
            .repository_base_path()
            .and_then(|base_path| references.get(&base_path).cloned());
        handles.push(runtime.spawn(test_single_mirror(
            mirror,
            client.clone(),
            reference,
            Arc::clone(&config),
            Arc::clone(&semaphore),
            Arc::clone(&freshness_semaphore),
            mpsc::Sender::clone(&tx_progress),
            check_freshness,
        )));
    }

    runtime
        .block_on(join_all(handles))
        .into_iter()
        .filter_map(|r| r.ok())
        .filter_map(|r| {
            // // USEFUL FOR DEBUGGING
            // if let Err(e) = r.as_ref() {
            //     println!("DEBUG => {:#?}", e);
            // }
            r.ok()
        })
        .collect()
}

fn rate_country_link<T>(
    map: &HashMap<&Country, Vec<T>>,
    link: &LinkTo,
    strategy: &RateStrategy,
) -> f64 {
    let country = Country::from_str(link.code).unwrap();
    let mirrors_score = match map.get(country) {
        Some(mirrors) => mirrors.len(),
        None => 0,
    };
    let distance_score = match link.link_type {
        LinkType::Submarine => (1. / link.distance).powf(1.),
        LinkType::Terrestrial => (1. / link.distance).powf(0.9),
    } * 15000.;
    match strategy {
        RateStrategy::HubsFirst => {
            (country.cable_connections_number as f64 * 1000.
                + country.internet_exchanges_number as f64)
                * mirrors_score as f64
        }
        RateStrategy::DistanceFirst => distance_score * mirrors_score as f64,
    }
}

pub fn test_speed_by_countries(
    mirrors: Vec<Mirror>,
    config: Arc<Config>,
    tx_progress: mpsc::Sender<String>,
    tx_results: mpsc::Sender<SpeedTestResults>,
) {
    let mut references: HashMap<String, Result<Arc<freshness::PackageBuildDates>, String>> =
        HashMap::new();
    let freshness_enabled =
        config.freshness_check.is_enabled() && config.target.supports_freshness();
    if freshness_enabled {
        for mirror in &mirrors {
            if let Some(base_path) = mirror.repository_base_path() {
                references.entry(base_path.clone()).or_insert_with(|| {
                    freshness::load_reference_db(
                        &config.ref_local_dir,
                        &freshness::reference_db_filename(&base_path),
                    )
                    .map(Arc::new)
                });
            }
        }
    }
    let mut map: HashMap<&'static Country, Vec<Mirror>> = HashMap::with_capacity(mirrors.len());
    let mut unlabeled_mirrors: Vec<Mirror> = Vec::new();
    for mirror in mirrors.into_iter() {
        match mirror.country {
            Some(country) => {
                map.entry(country).or_insert_with(Vec::new).push(mirror);
            }
            None => {
                unlabeled_mirrors.push(mirror);
            }
        }
    }
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let client = default_client_builder().expect("failed to build HTTP client");
    let semaphore = Arc::new(tokio::sync::Semaphore::new(config.concurrency));
    let freshness_semaphore = Arc::new(tokio::sync::Semaphore::new(8));

    let mut countries_to_check: Vec<&Country> = Vec::new();
    let mut speed_test_results: Vec<SpeedTestResult> = Vec::new();
    let mut tested_urls: HashSet<String> = HashSet::new();
    let mut visited_countries: HashSet<&'static str> = HashSet::new();
    let mut explored_countries: HashSet<&'static str> = HashSet::new();
    let mut jumps_number: usize = 0;

    let country = match Country::from_str(&config.entry_country) {
        Some(country) => country,
        None => {
            tx_progress
                .send("UNKNOWN entry_country, falling back to US".to_string())
                .unwrap();
            Country::from_str("US").unwrap()
        }
    };
    countries_to_check.push(country);

    let mut latest_top_speeds: Vec<f64> = Vec::with_capacity(config.max_jumps);
    let mut latest_top_connection_times: Vec<Duration> = Vec::with_capacity(config.max_jumps);

    while !countries_to_check.is_empty() {
        tx_progress
            .send(format!("JUMP #{}", jumps_number + 1))
            .unwrap();
        let current_countries = countries_to_check;
        countries_to_check = Vec::new();

        let mirrors_to_check: Vec<Mirror> = current_countries
            .into_iter()
            .map(|country| {
                let explored = explored_countries.contains(country.code);
                let visited = visited_countries.contains(country.code);
                if !explored {
                    tx_progress
                        .send(format!("EXPLORING {}", country.code))
                        .unwrap();
                    explored_countries.insert(country.code);
                }
                let mirrors_of_country = if visited {
                    Vec::new()
                } else {
                    tx_progress
                        .send(format!("VISITED {}", country.code))
                        .unwrap();
                    visited_countries.insert(country.code);
                    map.get(country)
                        .map(|mirrors| {
                            mirrors
                                .iter()
                                .take(config.country_test_mirrors_per_country)
                                .cloned()
                        })
                        .into_iter()
                        .flatten()
                        .collect()
                };

                let mut links: Vec<_> = if !explored {
                    country
                        .links
                        .iter()
                        .filter(|link| !config.is_country_excluded(link.code))
                        .collect()
                } else {
                    Vec::new()
                };
                let mut mirrors_of_neighbors = Vec::new();
                for strategy in [RateStrategy::DistanceFirst, RateStrategy::HubsFirst]
                    .iter()
                    .take(cmp::max(1, 3 - jumps_number as i8) as usize)
                    .rev()
                {
                    links.sort_unstable_by(|a, b| {
                        rate_country_link(&map, b, strategy)
                            .partial_cmp(&rate_country_link(&map, a, strategy))
                            .unwrap()
                    });
                    let mirrors = links
                        .iter()
                        .filter_map(|link| {
                            if !visited_countries.contains(link.code) {
                                let neighbor = Country::from_str(link.code);
                                neighbor?;
                                let neighbor = neighbor.unwrap();
                                visited_countries.insert(neighbor.code);
                                let mirrors = map
                                    .get(neighbor)
                                    .map(|mirrors| {
                                        mirrors
                                            .iter()
                                            .take(config.country_test_mirrors_per_country)
                                            .cloned()
                                    })
                                    .filter(|mirrors| mirrors.len() > 0);
                                if mirrors.is_some() {
                                    tx_progress
                                        .send(format!(
                                            "    + NEIGHBOR {} (by {:?})",
                                            link.code, strategy
                                        ))
                                        .unwrap();
                                    return mirrors;
                                }
                            }
                            None
                        })
                        .take(config.country_neighbors_per_country)
                        .flatten();
                    for mirror in mirrors {
                        mirrors_of_neighbors.push(mirror);
                    }
                }
                mirrors_of_country
                    .into_iter()
                    .chain(mirrors_of_neighbors.into_iter())
            })
            .flatten()
            .collect();

        tested_urls.extend(mirrors_to_check.iter().map(|m| m.url_to_test.to_string()));

        let mut results = test_mirrors(
            mirrors_to_check,
            client.clone(),
            &references,
            Arc::clone(&config),
            &runtime,
            Arc::clone(&semaphore),
            Arc::clone(&freshness_semaphore),
            mpsc::Sender::clone(&tx_progress),
            freshness_enabled,
        );
        jumps_number += 1;

        if results.is_empty() {
            tx_progress.send("BLANK ITERATION".to_string()).unwrap();
            break;
        }

        results.sort_unstable_by(|a, b| a.connection_time.partial_cmp(&b.connection_time).unwrap());
        for (index, result) in results.iter().enumerate() {
            let top_country = result.item.country.unwrap();
            let is_neighbor = !explored_countries.contains(top_country.code);
            if is_neighbor {
                tx_progress
                    .send(format!(
                        "    TOP NEIGHBOR - CONNECTION TIME: {} - {}",
                        top_country.code,
                        result.fmt_connection_time(),
                    ))
                    .unwrap();
                countries_to_check.push(top_country);
                latest_top_connection_times.push(result.connection_time);
                break;
            } else if index == 0 {
                tx_progress
                    .send(format!(
                        "    TOP CONNECTION TIME: {} - {}",
                        top_country.code,
                        result.fmt_connection_time(),
                    ))
                    .unwrap();
                latest_top_connection_times.push(result.connection_time);
            }
        }

        results.sort_unstable_by(|a, b| b.speed.partial_cmp(&a.speed).unwrap());
        for (index, result) in results.iter().enumerate() {
            let top_country = result.item.country.unwrap();
            let is_neighbor = !explored_countries.contains(top_country.code);
            if is_neighbor {
                tx_progress
                    .send(format!(
                        "    TOP NEIGHBOR - SPEED: {} - {}",
                        top_country.code,
                        result.fmt_speed(),
                    ))
                    .unwrap();
                countries_to_check.push(top_country);
                latest_top_speeds.push(result.speed);
                break;
            } else if index == 0 {
                tx_progress
                    .send(format!(
                        "    TOP SPEED: {} - {}",
                        top_country.code,
                        result.fmt_speed(),
                    ))
                    .unwrap();
                latest_top_speeds.push(result.speed);
            }
        }

        speed_test_results = speed_test_results
            .into_iter()
            .merge_by(results.into_iter(), |a, b| a.speed > b.speed)
            .collect();

        if jumps_number == config.max_jumps {
            break;
        }

        // === EARLY STOP CHECKS ===
        let connection_time_checks = 2;
        let speed_checks = 3;
        let speed_check_sensitivity = 1.2;
        let connection_time_check_sensitivity = 1.5;
        // BY CONNECTION TIME
        let connection_times_state: Vec<bool> = latest_top_connection_times
            .iter()
            .rev()
            .zip(latest_top_connection_times.iter().rev().skip(1))
            .map(|(next, prev)| {
                next.as_secs_f64() > prev.as_secs_f64() * connection_time_check_sensitivity
            })
            .take(connection_time_checks)
            .collect();
        if connection_times_state.len() == connection_time_checks
            && connection_times_state.iter().all(|b| *b)
        {
            tx_progress
                .send("CONNECTION TIMES ARE GETTING WORSE, STOPPING".to_string())
                .unwrap();
            break;
        }

        // BY SPEED
        let speeds_state: Vec<bool> = latest_top_speeds
            .iter()
            .rev()
            .zip(latest_top_speeds.iter().rev().skip(1))
            .map(|(next, prev)| *next as f64 * speed_check_sensitivity < *prev as f64)
            .take(speed_checks)
            .collect();
        if speeds_state.len() == speed_checks && speeds_state.iter().all(|b| *b) {
            tx_progress
                .send("SPEEDS ARE GETTING WORSE, STOPPING".to_string())
                .unwrap();
            break;
        }

        tx_progress.send(format!("")).unwrap();
    }

    if speed_test_results.len()
        < ((config.max_jumps
            * config.country_test_mirrors_per_country
            * config.country_neighbors_per_country) as f64
            * 0.7) as usize
    {
        tx_progress
            .send(format!(
                "COUNTRY JUMPING YIELDED TOO FEW MIRRORS ({}), ADDING OTHERS TO UNLABELED",
                speed_test_results.len()
            ))
            .unwrap();
        for mirrors in map.into_values() {
            let mut untested_mirrors: Vec<Mirror> = mirrors
                .into_iter()
                .filter(|m| !tested_urls.contains(m.url_to_test.as_str()))
                .collect();
            unlabeled_mirrors.append(&mut untested_mirrors);
        }
    }

    if !unlabeled_mirrors.is_empty() {
        tx_progress.send("\n".to_string()).unwrap();
        tx_progress
            .send("TESTING UNLABELED MIRRORS".to_string())
            .unwrap();

        let semaphore_for_unlabeled = Arc::new(tokio::sync::Semaphore::new(
            config.concurrency_for_unlabeled,
        ));
        let mut results = test_mirrors(
            unlabeled_mirrors,
            client.clone(),
            &references,
            Arc::clone(&config),
            &runtime,
            Arc::clone(&semaphore_for_unlabeled),
            Arc::clone(&freshness_semaphore),
            mpsc::Sender::clone(&tx_progress),
            freshness_enabled,
        );

        results.sort_unstable_by(|a, b| b.speed.partial_cmp(&a.speed).unwrap());
        speed_test_results = speed_test_results
            .into_iter()
            .merge_by(results.into_iter(), |a, b| a.speed > b.speed)
            .collect();
    }

    tx_progress.send("\n".to_string()).unwrap();
    if speed_test_results.is_empty() {
        tx_progress
            .send("NO RESULTS TO RE-TEST".to_string())
            .unwrap();
        return;
    }

    // The freshness path checks every speed-tested mirror before any final
    // top-N split, so slower but fresher candidates are not discarded.
    let mut top_mirror_results = speed_test_results;

    // Check every speed-verified mirror's DB before selecting retest candidates.
    if freshness_enabled
        && top_mirror_results
            .iter()
            .any(|result| result.item.repository_base_path().is_some())
    {
        let frontier = freshness::build_frontier(
            references
                .values()
                .filter_map(|reference| reference.as_ref().ok().map(Arc::as_ref))
                .chain(
                    top_mirror_results
                        .iter()
                        .filter_map(|result| result.freshness_packages.as_ref()),
                ),
        );

        for result in &mut top_mirror_results {
            if let Some(packages) = result.freshness_packages.take() {
                let (age_days, missing, lag_seconds) =
                    freshness::calculate_relative_freshness_score(&packages, &frontier);
                let quality = freshness_quality(age_days, missing, frontier.packages.len());
                result.freshness_quality = Some(quality);
                result.latest_build_date = packages.latest_build_date();
                let latest_build_display = result
                    .latest_build_date
                    .and_then(|timestamp| {
                        chrono::DateTime::<chrono::Utc>::from_timestamp(timestamp, 0)
                    })
                    .map(|date| date.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                tx_progress
                    .send(format!(
                        "    {} peer build age: {:+.4} days; freshness quality: {:.4} ({} missing, {}s total build lag; latest package build: {})",
                        result.item.url,
                        age_days,
                        quality,
                        missing,
                        lag_seconds,
                        latest_build_display
                    ))
                    .unwrap();
            }
        }

        let failed = top_mirror_results
            .iter()
            .filter(|result| result.freshness_quality.is_none())
            .count();
        top_mirror_results.retain(|result| result.freshness_quality.is_some());
        sort_by_weighted(
            &mut top_mirror_results,
            config.freshness_check.speed_weight(),
        );
        tx_progress
            .send(format!(
                "FRESHNESS CHECK COMPLETE: {} verified, {} excluded (speed weight {:.2})",
                top_mirror_results.len(),
                failed,
                config.freshness_check.speed_weight()
            ))
            .unwrap();

        let retest_count = cmp::min(
            config.top_mirrors_number_to_retest,
            top_mirror_results.len(),
        );
        if retest_count > 0 {
            tx_progress
                .send(format!(
                    "RE-TESTING TOP MIRRORS ({} selected by weighted speed and freshness)",
                    retest_count
                ))
                .unwrap();
            let selected: Vec<_> = top_mirror_results.drain(..retest_count).collect();
            let selected_mirrors = selected.iter().map(|result| result.item.clone());
            let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
            // Only repeat the .files speed probe; the verified DB and its
            // package scores are retained from the initial pass.
            let retested = test_mirrors(
                selected_mirrors,
                client,
                &references,
                Arc::clone(&config),
                &runtime,
                semaphore,
                freshness_semaphore,
                mpsc::Sender::clone(&tx_progress),
                false,
            );
            let (ranked, failed) = merge_freshness_retests(
                top_mirror_results,
                selected,
                retested,
                config.freshness_check.speed_weight(),
            );
            top_mirror_results = ranked;
            tx_progress
                .send(format!(
                    "RETEST COMPLETE: {} passed, {} failed; ranking with files-only speeds",
                    retest_count - failed,
                    failed
                ))
                .unwrap();
        }
    } else {
        let mut other_results = top_mirror_results.split_off(cmp::min(
            config.top_mirrors_number_to_retest,
            top_mirror_results.len(),
        ));
        tx_progress
            .send("RE-TESTING TOP MIRRORS".to_string())
            .unwrap();
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let top_mirrors = top_mirror_results.into_iter().map(|result| result.item);
        top_mirror_results = test_mirrors(
            top_mirrors,
            client,
            &references,
            Arc::clone(&config),
            &runtime,
            semaphore,
            freshness_semaphore,
            mpsc::Sender::clone(&tx_progress),
            false,
        );
        top_mirror_results.sort_by(|a, b| b.speed.partial_cmp(&a.speed).unwrap());
        top_mirror_results.append(&mut other_results);
    }

    tx_results.send(top_mirror_results).unwrap();

    // Drop channels before shutting down the runtime. Without this runtime drop can block
    // indefinitely (e.g. reqwest connection-pool cleanup)
    drop(tx_progress);
    drop(tx_results);
    runtime.shutdown_timeout(Duration::from_secs(1));
}

fn freshness_quality(age_days: f64, missing: usize, total: usize) -> f64 {
    if total == 0 {
        return 0.0;
    }
    let coverage = (total - missing.min(total)) as f64 / total as f64;
    // One average day behind halves freshness. Missing packages lower it
    // independently; a mirror matching the full frontier scores exactly 1.
    coverage / (1.0 + (-age_days).max(0.0))
}

fn include_db_transfer_in_speed(result: &mut SpeedTestResult, transfer: &freshness::TransferStats) {
    result.bytes_downloaded = result.bytes_downloaded.saturating_add(transfer.bytes);
    result.elapsed = result.elapsed.saturating_add(transfer.elapsed);
    if !result.elapsed.is_zero() {
        result.speed = result.bytes_downloaded as f64 / result.elapsed.as_secs_f64();
    }
}

fn normalized_speed(speed: f64, fastest: f64) -> f64 {
    if fastest > 0.0 && fastest.is_finite() && speed.is_finite() {
        (speed / fastest).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn sort_by_weighted(results: &mut SpeedTestResults, speed_weight: f64) {
    let fastest = results
        .iter()
        .filter(|result| result.freshness_quality.is_some())
        .map(|result| result.speed)
        .filter(|speed| speed.is_finite())
        .fold(0.0_f64, f64::max);
    results.sort_by(|a, b| compare_weighted(a, b, speed_weight, fastest));
}

fn merge_freshness_retests(
    mut remaining: SpeedTestResults,
    selected: SpeedTestResults,
    retested: SpeedTestResults,
    speed_weight: f64,
) -> (SpeedTestResults, usize) {
    let mut selected_by_url: HashMap<String, SpeedTestResult> = selected
        .into_iter()
        .map(|result| (result.item.url.to_string(), result))
        .collect();
    // After a files-only retest, all other candidates use files-only speed
    // too; comparing it against their pooled files+DB speed would be unfair.
    for result in &mut remaining {
        result.use_files_measurement();
    }
    for retest in retested {
        if let Some(mut original) = selected_by_url.remove(retest.item.url.as_str()) {
            original.use_retest_measurement(&retest);
            remaining.push(original);
        }
    }
    let failed = selected_by_url.len();
    sort_by_weighted(&mut remaining, speed_weight);
    (remaining, failed)
}

fn compare_weighted(
    a: &SpeedTestResult,
    b: &SpeedTestResult,
    speed_weight: f64,
    fastest: f64,
) -> std::cmp::Ordering {
    let a_freshness = a.freshness_quality.unwrap_or(0.0);
    let b_freshness = b.freshness_quality.unwrap_or(0.0);
    let a_speed = normalized_speed(a.speed, fastest);
    let b_speed = normalized_speed(b.speed, fastest);
    let a_combined = speed_weight * a_speed + (1.0 - speed_weight) * a_freshness;
    let b_combined = speed_weight * b_speed + (1.0 - speed_weight) * b_freshness;

    b.freshness_quality
        .is_some()
        .cmp(&a.freshness_quality.is_some())
        .then_with(|| b_combined.total_cmp(&a_combined))
        .then_with(|| {
            if speed_weight == 1.0 {
                b_freshness.total_cmp(&a_freshness)
            } else {
                b_speed.total_cmp(&a_speed)
            }
        })
        .then_with(|| b_freshness.total_cmp(&a_freshness))
        .then_with(|| b.latest_build_date.cmp(&a.latest_build_date))
        .then_with(|| a.item.url.as_str().cmp(b.item.url.as_str()))
}

#[cfg(test)]
mod freshness_order_tests {
    use super::*;

    fn result(url: &str, speed: usize, quality: Option<f64>) -> SpeedTestResult {
        let url = url::Url::parse(url).unwrap();
        let mirror = Mirror {
            url: url.clone(),
            url_to_test: url,
            country: None,
        };
        let mut result = SpeedTestResult::new(
            mirror,
            speed,
            Duration::from_secs(1),
            Duration::from_millis(10),
        );
        result.freshness_quality = quality;
        result
    }

    #[test]
    fn freshness_quality_accounts_for_age_and_missing_packages() {
        assert_eq!(freshness_quality(0.0, 0, 2), 1.0);
        assert_eq!(freshness_quality(-1.0, 0, 2), 0.5);
        assert_eq!(freshness_quality(0.0, 1, 2), 0.5);
        assert_eq!(freshness_quality(-1.0, 1, 2), 0.25);
    }

    #[test]
    fn freshness_mode_speed_uses_both_files_and_db_transfer() {
        let mut candidate = result("https://example.org/", 100_000, Some(1.0));
        let db = freshness::TransferStats {
            bytes: 50_000,
            elapsed: Duration::from_secs(2),
        };
        include_db_transfer_in_speed(&mut candidate, &db);
        assert_eq!(candidate.bytes_downloaded, 150_000);
        assert_eq!(candidate.elapsed, Duration::from_secs(3));
        assert_eq!(candidate.speed, 50_000.0);
        candidate.use_files_measurement();
        assert_eq!(candidate.bytes_downloaded, 100_000);
        assert_eq!(candidate.elapsed, Duration::from_secs(1));
        assert_eq!(candidate.speed, 100_000.0);
    }

    #[test]
    fn weighted_shortlist_retests_speed_and_preserves_freshness() {
        let mut fast = result("https://fast.example/", 100, Some(0.5));
        let mut fresh = result("https://fresh.example/", 90, Some(1.0));
        include_db_transfer_in_speed(
            &mut fast,
            &freshness::TransferStats {
                bytes: 200,
                elapsed: Duration::from_secs(1),
            },
        );
        include_db_transfer_in_speed(
            &mut fresh,
            &freshness::TransferStats {
                bytes: 90,
                elapsed: Duration::from_secs(1),
            },
        );
        let mut initial = vec![fast, fresh];
        sort_by_weighted(&mut initial, 0.5);
        assert_eq!(initial[0].item.url.as_str(), "https://fresh.example/");
        let selected = vec![initial.remove(0)];
        let retest = result("https://fresh.example/", 40, None);
        let (final_results, failed) = merge_freshness_retests(initial, selected, vec![retest], 0.5);
        assert_eq!(failed, 0);
        assert_eq!(final_results[0].item.url.as_str(), "https://fast.example/");
        assert_eq!(final_results[0].speed, 100.0);
        assert_eq!(final_results[1].speed, 40.0);
        assert_eq!(final_results[1].freshness_quality, Some(1.0));
    }

    #[test]
    fn failed_retest_removes_only_that_candidate() {
        let selected = vec![result("https://failed.example/", 100, Some(1.0))];
        let remaining = vec![result("https://okay.example/", 80, Some(0.5))];
        let (final_results, failed) = merge_freshness_retests(remaining, selected, vec![], 0.5);
        assert_eq!(failed, 1);
        assert_eq!(final_results.len(), 1);
        assert_eq!(final_results[0].item.url.as_str(), "https://okay.example/");
    }

    #[test]
    fn weight_endpoints_use_speed_or_freshness_as_primary() {
        let fast = result("https://fast.example/", 100, Some(0.2));
        let fresh = result("https://fresh.example/", 90, Some(1.0));
        assert_eq!(
            compare_weighted(&fast, &fresh, 1.0, 100.0),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_weighted(&fast, &fresh, 0.0, 100.0),
            std::cmp::Ordering::Greater
        );

        let equally_fast = result("https://equal.example/", 100, Some(1.0));
        assert_eq!(
            compare_weighted(&equally_fast, &fast, 1.0, 100.0),
            std::cmp::Ordering::Less
        );
        let equally_fresh = result("https://equal.example/", 80, Some(1.0));
        assert_eq!(
            compare_weighted(&fresh, &equally_fresh, 0.0, 100.0),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn intermediate_weight_allows_freshness_to_overcome_a_small_speed_gap() {
        let fast = result("https://fast.example/", 100, Some(0.0));
        let fresh = result("https://fresh.example/", 95, Some(1.0));
        assert_eq!(
            compare_weighted(&fresh, &fast, 0.9, 100.0),
            std::cmp::Ordering::Less
        );

        let slower = result("https://slower.example/", 50, Some(1.0));
        assert_eq!(
            compare_weighted(&fast, &slower, 0.9, 100.0),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn failed_freshness_checks_rank_after_verified_mirrors() {
        let failed = result("https://failed.example/", 200, None);
        let verified = result("https://verified.example/", 50, Some(0.5));
        assert_eq!(
            compare_weighted(&verified, &failed, 1.0, 200.0),
            std::cmp::Ordering::Less
        );
    }
}
