use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use flate2::read::GzDecoder;
use reqwest::Client;
use tar::Archive;
use tokio::time::timeout;
use url::Url;
use zstd::stream::read::Decoder as ZstdDecoder;

#[derive(Debug, Clone)]
pub struct FreshnessCheckResult {
    pub score: f64,
    pub packages_compared: usize,
    pub reference_packages: usize,
    pub error: Option<String>,
    pub packages: Option<PackageBuildDates>,
    pub transfer: Option<TransferStats>,
}

#[derive(Debug, Clone)]
pub struct TransferStats {
    pub bytes: usize,
    pub elapsed: Duration,
}

impl FreshnessCheckResult {
    fn failed(error: impl Into<String>) -> Self {
        Self {
            score: 0.0,
            packages_compared: 0,
            reference_packages: 0,
            error: Some(error.into()),
            packages: None,
            transfer: None,
        }
    }

    pub fn reference_error(error: String) -> Self {
        Self::failed(format!("ref db error: {}", error))
    }
}

const MAX_DB_BYTES: usize = 64 * 1024 * 1024;
const MAX_UNPACKED_DB_BYTES: u64 = 256 * 1024 * 1024;
const MAX_DESC_BYTES: u64 = 1024 * 1024;
const SECONDS_PER_DAY: f64 = 24.0 * 60.0 * 60.0;

#[derive(Debug, Clone)]
pub struct PackageBuildDates {
    pub packages: HashMap<String, i64>,
}

impl PackageBuildDates {
    pub fn latest_build_date(&self) -> Option<i64> {
        self.packages.values().copied().max()
    }
}

pub async fn check_mirror(
    client: Client,
    mirror_url: Url,
    base_path: &str,
    reference: Arc<PackageBuildDates>,
    timeout_ms: u64,
) -> FreshnessCheckResult {
    let db_url: Url = match mirror_url.join(&format!("{}.db", base_path)) {
        Ok(u) => u,
        Err(e) => return FreshnessCheckResult::failed(format!("failed to build db url: {}", e)),
    };

    let fetch = async {
        let mut resp = client
            .get(db_url.clone())
            .timeout(Duration::from_millis(timeout_ms))
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?;
        if resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/html"))
        {
            return Err("mirror returned HTML instead of a package database".to_string());
        }
        if resp
            .content_length()
            .is_some_and(|size| size > MAX_DB_BYTES as u64)
        {
            return Err("package database exceeds download size limit".to_string());
        }
        let body_started = Instant::now();
        let mut bytes = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
            if chunk.len() > MAX_DB_BYTES - bytes.len() {
                return Err("package database exceeds download size limit".to_string());
            }
            bytes.extend_from_slice(&chunk);
        }
        let transfer = TransferStats {
            bytes: bytes.len(),
            elapsed: body_started.elapsed(),
        };
        Ok::<_, String>((bytes, transfer))
    };

    let (mirror_bytes, transfer) = match timeout(Duration::from_millis(timeout_ms), fetch).await {
        Ok(Ok(result)) => result,
        Ok(Err(e)) => return FreshnessCheckResult::failed(format!("download error: {}", e)),
        Err(_) => return FreshnessCheckResult::failed("download timeout"),
    };

    evaluate_downloaded_db(&mirror_bytes, transfer, &reference)
}

fn evaluate_downloaded_db(
    mirror_bytes: &[u8],
    transfer: TransferStats,
    reference: &PackageBuildDates,
) -> FreshnessCheckResult {
    if looks_like_html(mirror_bytes) {
        return FreshnessCheckResult::failed("mirror returned HTML instead of a package database");
    }

    let mirror_pkgs = match parse_db_bytes(mirror_bytes) {
        Ok(p) => p,
        Err(e) => return FreshnessCheckResult::failed(format!("parse error: {}", e)),
    };

    let (score, compared) = calculate_freshness_score(&mirror_pkgs, reference);
    if compared == 0 {
        return FreshnessCheckResult::failed("no packages overlap with the local reference");
    }

    FreshnessCheckResult {
        score,
        packages_compared: compared,
        reference_packages: reference.packages.len(),
        error: None,
        packages: Some(mirror_pkgs),
        transfer: Some(transfer),
    }
}

pub fn reference_db_filename(base_path: &str) -> String {
    match Path::new(base_path).file_name() {
        Some(name) => name.to_string_lossy().to_string() + ".db",
        None => "mirror.db".to_string(),
    }
}

fn looks_like_html(data: &[u8]) -> bool {
    let prefix = data
        .iter()
        .take(64)
        .copied()
        .skip_while(|byte| byte.is_ascii_whitespace())
        .collect::<Vec<_>>();
    let prefix = String::from_utf8_lossy(&prefix).to_ascii_lowercase();
    prefix.starts_with("<!doctype html") || prefix.starts_with("<html")
}

pub fn load_reference_db(dir: &str, db_filename: &str) -> Result<PackageBuildDates, String> {
    let path: PathBuf = Path::new(dir).join(db_filename);
    let data =
        std::fs::read(&path).map_err(|e| format!("read ref db {}: {}", path.display(), e))?;
    parse_db_bytes(&data)
}

fn parse_db_bytes(data: &[u8]) -> Result<PackageBuildDates, String> {
    if data.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        let decoder = ZstdDecoder::new(data).map_err(|e| e.to_string())?;
        return parse_tar(decoder.take(MAX_UNPACKED_DB_BYTES + 1));
    }
    if data.starts_with(&[0x1f, 0x8b]) {
        return parse_tar(GzDecoder::new(data).take(MAX_UNPACKED_DB_BYTES + 1));
    }
    parse_tar(data)
}

fn parse_tar(reader: impl Read) -> Result<PackageBuildDates, String> {
    let mut pkgs = HashMap::new();
    let mut archive = Archive::new(reader);
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?;
        let name_str = path.to_str().map(|s| s.to_string());
        if let Some(name) = name_str {
            if !name.ends_with("/desc") && name != "desc" {
                continue;
            }
            if entry.size() > MAX_DESC_BYTES {
                return Err("package description exceeds size limit".to_string());
            }
            let mut contents = Vec::new();
            entry
                .read_to_end(&mut contents)
                .map_err(|e| e.to_string())?;
            if let (Some(pkg_name), Some(ts)) = (
                extract_field(&contents, "%NAME%"),
                extract_build_date(&contents),
            ) {
                pkgs.insert(pkg_name, ts);
            }
        }
    }
    if pkgs.is_empty() {
        Err("database contains no usable package descriptions".to_string())
    } else {
        Ok(PackageBuildDates { packages: pkgs })
    }
}

fn extract_build_date(desc: &[u8]) -> Option<i64> {
    extract_field(desc, "%BUILDDATE%")?.parse::<i64>().ok()
}

fn extract_field(desc: &[u8], field: &str) -> Option<String> {
    let text = String::from_utf8_lossy(desc);
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if line.trim() == field {
            return lines
                .next()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
        }
    }
    None
}

pub fn calculate_freshness_score(
    mirror: &PackageBuildDates,
    reference: &PackageBuildDates,
) -> (f64, usize) {
    let mut age_days = 0.0;
    let mut compared = 0;
    for (pkg, ref_ts) in reference.packages.iter() {
        if let Some(m_ts) = mirror.packages.get(pkg) {
            compared += 1;
            age_days += build_date_difference_days(*m_ts, *ref_ts);
        }
    }
    if compared == 0 {
        (0.0, 0)
    } else {
        (age_days / compared as f64, compared)
    }
}

fn build_date_difference_days(mirror_date: i64, reference_date: i64) -> f64 {
    let difference = (mirror_date as i128 - reference_date as i128) as f64;
    difference / SECONDS_PER_DAY
}

pub fn build_frontier<'a>(
    mirrors: impl IntoIterator<Item = &'a PackageBuildDates>,
) -> PackageBuildDates {
    let mut frontier = HashMap::new();
    for mirror in mirrors {
        for (name, build_date) in &mirror.packages {
            frontier
                .entry(name.clone())
                .and_modify(|newest: &mut i64| *newest = (*newest).max(*build_date))
                .or_insert(*build_date);
        }
    }
    PackageBuildDates { packages: frontier }
}

pub fn calculate_relative_freshness_score(
    mirror: &PackageBuildDates,
    frontier: &PackageBuildDates,
) -> (f64, usize, u64) {
    if frontier.packages.is_empty() {
        return (0.0, 0, 0);
    }
    let mut age_days = 0.0;
    let mut compared = 0;
    let mut missing = 0;
    let mut lag_seconds = 0_u64;
    for (name, newest) in &frontier.packages {
        if let Some(build_date) = mirror.packages.get(name) {
            age_days += build_date_difference_days(*build_date, *newest);
            compared += 1;
            if build_date < newest {
                lag_seconds = lag_seconds.saturating_add(newest.saturating_sub(*build_date) as u64);
            }
        } else {
            missing += 1;
        }
    }
    (
        if compared == 0 {
            0.0
        } else {
            age_days / compared as f64
        },
        missing,
        lag_seconds,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn packages(entries: &[(&str, i64)]) -> PackageBuildDates {
        PackageBuildDates {
            packages: entries
                .iter()
                .map(|(name, timestamp)| ((*name).to_string(), *timestamp))
                .collect(),
        }
    }

    fn database_with_package(directory: &str, package_name: &str, timestamp: i64) -> Vec<u8> {
        let desc = format!("%NAME%\n{}\n\n%BUILDDATE%\n{}\n", package_name, timestamp);
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(desc.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, format!("{}/desc", directory), desc.as_bytes())
            .unwrap();
        builder.into_inner().unwrap()
    }

    #[test]
    fn build_date_parser_reads_pacman_desc_field() {
        let desc = b"%NAME%\nexample\n\n%BUILDDATE%\n1720000000\n";
        assert_eq!(extract_build_date(desc), Some(1_720_000_000));
    }

    #[test]
    fn build_date_parser_rejects_missing_or_invalid_values() {
        assert_eq!(extract_build_date(b"%NAME%\nexample\n"), None);
        assert_eq!(extract_build_date(b"%BUILDDATE%\nnot-a-number\n"), None);
    }

    #[test]
    fn package_identity_comes_from_name_field_not_versioned_tar_directory() {
        let older =
            parse_db_bytes(&database_with_package("example-1.0-1", "example", 100)).unwrap();
        let newer =
            parse_db_bytes(&database_with_package("example-2.0-1", "example", 200)).unwrap();
        assert_eq!(older.packages.get("example"), Some(&100));
        assert_eq!(newer.packages.get("example"), Some(&200));
        assert!(calculate_freshness_score(&older, &newer).0 < 0.0);
        assert!(calculate_freshness_score(&newer, &older).0 > 0.0);
    }

    #[test]
    fn score_scales_with_how_much_newer_or_older_packages_are() {
        let reference = packages(&[("newer", 100_000), ("equal", 100_000), ("older", 100_000)]);
        let mirror = packages(&[("newer", 186_400), ("equal", 100_000), ("older", 56_800)]);
        let (score, compared) = calculate_freshness_score(&mirror, &reference);
        assert_eq!(compared, 3);
        // +1 day, equal, and -12 hours average to +1/6 day.
        assert!((score - 1.0 / 6.0).abs() < 1e-12);

        let one_hour_behind = packages(&[("example", 96_400)]);
        let one_day_behind = packages(&[("example", 13_600)]);
        let baseline = packages(&[("example", 100_000)]);
        assert!(
            calculate_freshness_score(&one_hour_behind, &baseline).0
                > calculate_freshness_score(&one_day_behind, &baseline).0
        );
        assert_eq!(
            calculate_freshness_score(&one_day_behind, &baseline).0,
            -1.0
        );
        let two_point_four_hours_ahead = packages(&[("example", 108_640)]);
        assert!(
            (calculate_freshness_score(&two_point_four_hours_ahead, &baseline).0 - 0.1).abs()
                < 1e-12
        );
    }

    #[test]
    fn missing_local_packages_are_counted_separately_from_age() {
        let reference = packages(&[("present", 10), ("missing", 20)]);
        let mirror = packages(&[("present", 10)]);
        assert_eq!(calculate_freshness_score(&mirror, &reference), (0.0, 1));
    }

    #[test]
    fn score_is_zero_when_no_packages_overlap() {
        let reference = packages(&[("reference-only", 10)]);
        let mirror = packages(&[("mirror-only", 20)]);
        assert_eq!(calculate_freshness_score(&mirror, &reference), (0.0, 0));
    }

    #[test]
    fn peer_frontier_distinguishes_mirrors_both_current_against_local_db() {
        let reference = packages(&[("existing", 10)]);
        let older = packages(&[("existing", 10)]);
        let newer = packages(&[("existing", 11), ("new-package", 12)]);
        assert_eq!(calculate_freshness_score(&older, &reference).0, 0.0);
        assert!(calculate_freshness_score(&newer, &reference).0 > 0.0);

        let frontier = build_frontier([&older, &newer]);
        assert_eq!(
            calculate_relative_freshness_score(&newer, &frontier),
            (0.0, 0, 0)
        );
        let (score, missing, lag) = calculate_relative_freshness_score(&older, &frontier);
        assert!(score < 0.0);
        assert_eq!(missing, 1);
        assert_eq!(lag, 1);
        assert_eq!(older.latest_build_date(), Some(10));
        assert_eq!(newer.latest_build_date(), Some(12));
    }

    #[test]
    fn local_database_can_set_the_frontier_when_all_mirrors_lag() {
        let reference = packages(&[("example", 186_400)]);
        let mirror = packages(&[("example", 100_000)]);
        let frontier = build_frontier([&reference, &mirror]);
        assert_eq!(frontier.packages.get("example"), Some(&186_400));
        assert_eq!(
            calculate_relative_freshness_score(&mirror, &frontier),
            (-1.0, 0, 86_400)
        );
    }

    #[test]
    fn latest_build_date_is_not_influenced_by_package_iteration_order() {
        let mirror = packages(&[("newest", 300), ("older", 100), ("middle", 200)]);
        assert_eq!(mirror.latest_build_date(), Some(300));
        assert_eq!(packages(&[]).latest_build_date(), None);
    }

    #[test]
    fn peer_age_tracks_lag_and_reports_missing_packages_separately() {
        let frontier = packages(&[("a", 20), ("b", 20)]);
        let recently_behind = packages(&[("a", 19), ("b", 20)]);
        let far_behind = packages(&[("a", 10), ("b", 20)]);
        let incomplete = packages(&[("b", 20)]);
        let (recent_score, recent_missing, recent_lag) =
            calculate_relative_freshness_score(&recently_behind, &frontier);
        let (far_score, far_missing, far_lag) =
            calculate_relative_freshness_score(&far_behind, &frontier);
        assert_eq!((recent_missing, recent_lag), (0, 1));
        assert_eq!((far_missing, far_lag), (0, 10));
        assert!(recent_score > far_score);
        assert_eq!(
            calculate_relative_freshness_score(&incomplete, &frontier),
            (0.0, 1, 0)
        );
    }

    #[test]
    fn build_date_difference_handles_extreme_timestamps() {
        assert!(build_date_difference_days(i64::MAX, i64::MIN).is_finite());
        assert!(build_date_difference_days(i64::MIN, i64::MAX).is_finite());
        assert_eq!(build_date_difference_days(100, 100), 0.0);
        assert_eq!(build_date_difference_days(86_400, 0), 1.0);
    }

    #[test]
    fn reference_db_filename_uses_repo_basename() {
        assert_eq!(reference_db_filename("extra/os/x86_64/extra"), "extra.db");
        assert_eq!(
            reference_db_filename("x86_64/cachyos/cachyos"),
            "cachyos.db"
        );
    }

    #[test]
    fn html_and_empty_archives_are_not_package_databases() {
        assert!(looks_like_html(b"  <!DOCTYPE html>\n<html lang=\"ko\">"));
        assert!(looks_like_html(b"<html>error</html>"));
        assert!(parse_db_bytes(b"").is_err());
        assert!(parse_db_bytes(&[0_u8; 1024]).is_err());
    }

    #[test]
    fn parses_raw_zstd_and_gzip_databases() {
        let raw = database_with_package("example-1.0-1", "example", 100);
        let zstd = zstd::stream::encode_all(raw.as_slice(), 1).unwrap();
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gzip.write_all(&raw).unwrap();
        let gzip = gzip.finish().unwrap();
        for database in [&raw, &zstd, &gzip] {
            let parsed = parse_db_bytes(database).unwrap();
            assert_eq!(parsed.packages.get("example"), Some(&100));
        }
    }

    #[test]
    fn one_database_body_provides_both_build_dates_and_transfer_stats() {
        let payload = database_with_package("example-2.0-1", "example", 186_400);
        let transfer = TransferStats {
            bytes: payload.len(),
            elapsed: Duration::from_secs(1),
        };
        let reference = packages(&[("example", 100_000)]);
        let result = evaluate_downloaded_db(&payload, transfer, &reference);
        assert!(result.error.is_none(), "{:?}", result.error);
        assert_eq!(result.packages_compared, 1);
        assert_eq!(result.reference_packages, 1);
        assert_eq!(result.packages.unwrap().latest_build_date(), Some(186_400));
        assert_eq!(result.transfer.unwrap().bytes, payload.len());
        assert_eq!(result.score, 1.0);
    }
}
