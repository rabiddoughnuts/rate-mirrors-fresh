use crate::mirror::Mirror;
use crate::target_configs::archarm::ArcharmTarget;
use crate::target_configs::archlinux::ArchTarget;
use crate::target_configs::archlinuxcn::ArchCNTarget;
use crate::target_configs::arcolinux::ArcoLinuxTarget;
use crate::target_configs::artix::ArtixTarget;
use crate::target_configs::blackarch::BlackArchTarget;
use crate::target_configs::cachyos::CachyOSTarget;
use crate::target_configs::chaotic::ChaoticTarget;
// use crate::target_configs::debian::DebianTarget;
use crate::target_configs::endeavouros::EndeavourOSTarget;
use crate::target_configs::manjaro::ManjaroTarget;
use crate::target_configs::openbsd::OpenBSDTarget;
use crate::target_configs::rebornos::RebornOSTarget;
use crate::target_configs::stdin::StdinTarget;
// use crate::target_configs::ubuntu::UbuntuTarget;
use ambassador::{Delegate, delegatable_trait};
use clap::{Parser, Subcommand};
use itertools::Itertools;
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, mpsc};
use thiserror::Error;
use url::Url;

#[derive(Debug, PartialEq, Clone)]
pub enum Protocol {
    Http,
    Https,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FreshnessMode {
    Off,
    On(f64),
}

impl FreshnessMode {
    pub fn is_enabled(self) -> bool {
        matches!(self, Self::On(_))
    }

    pub fn speed_weight(self) -> f64 {
        match self {
            Self::Off => 1.0,
            Self::On(weight) => weight,
        }
    }
}

impl FromStr for FreshnessMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "off" | "false" => Ok(Self::Off),
            "on" | "true" => Ok(Self::On(1.0)),
            _ => parse_speed_weight(value).map(Self::On),
        }
    }
}

impl FromStr for Protocol {
    type Err = &'static str;
    fn from_str(protocol: &str) -> Result<Self, Self::Err> {
        match protocol {
            "http" => Ok(Protocol::Http),
            "https" => Ok(Protocol::Https),
            _ => Err("could not parse protocol"),
        }
    }
}

#[derive(Error)]
pub enum AppError {
    #[error("do not run rate-mirrors with root permissions")]
    Root,
    #[error("failed to connect to {0}, consider increasing fetch-mirrors-timeout")]
    RequestTimeout(String),
    #[error("{0}")]
    RequestError(String),
    #[error("no mirrors after filtering")]
    NoMirrorsAfterFiltering,
    #[error(
        "no mirrors passed the requested freshness check; existing mirrorlists were not replaced"
    )]
    NoFreshMirrorsVerified,
    #[error(transparent)]
    UrlParseError(#[from] url::ParseError),
    #[error(transparent)]
    IoError(#[from] std::io::Error),
}

impl fmt::Debug for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self)
    }
}

impl From<reqwest::Error> for AppError {
    fn from(err: reqwest::Error) -> AppError {
        if err.is_timeout() {
            AppError::RequestTimeout(err.url().map(|u| u.to_string()).unwrap_or_default())
        } else {
            AppError::RequestError(err.to_string())
        }
    }
}

#[delegatable_trait]
pub trait LogFormatter {
    fn format_comment(&self, message: impl fmt::Display) -> String;
    fn format_mirror(&self, mirror: &Mirror) -> String;
}

#[delegatable_trait]
pub trait FetchMirrors {
    fn fetch_mirrors(
        &self,
        config: Arc<Config>,
        tx_progress: mpsc::Sender<String>,
    ) -> Result<Vec<Mirror>, AppError>;
}

#[derive(Debug, Subcommand, Clone, Delegate)]
#[delegate(FetchMirrors)]
#[delegate(LogFormatter)]
pub enum Target {
    /// accepts lines of urls OR lines with tab-separated urls and countries
    Stdin(StdinTarget),

    /// test archlinux mirrors
    Arch(ArchTarget),

    /// test archlinuxcn mirrors
    #[command(name = "archlinuxcn")]
    ArchCN(ArchCNTarget),

    /// test archlinuxarm mirrors
    Archarm(ArcharmTarget),

    /// test artix mirrors
    Artix(ArtixTarget),

    /// test arcolinux mirrors
    #[command(name = "arcolinux")]
    ArcoLinux(ArcoLinuxTarget),

    /// test blackarch mirrors
    #[command(name = "blackarch")]
    BlackArch(BlackArchTarget),

    /// test cachyos mirrors
    #[command(name = "cachyos")]
    CachyOS(CachyOSTarget),

    /// test chaotic-aur mirrors
    #[command(name = "chaotic-aur")]
    Chaotic(ChaoticTarget),

    /// test endeavouros mirrors
    #[command(name = "endeavouros")]
    EndeavourOS(EndeavourOSTarget),

    /// test manjaro mirrors
    Manjaro(ManjaroTarget),

    /// test OpenBSD mirrors
    #[command(name = "openbsd")]
    OpenBSD(OpenBSDTarget),

    /// test rebornos mirrors
    #[command(name = "rebornos")]
    RebornOS(RebornOSTarget),
}

#[derive(Debug, Parser)]
#[command(
    name = "rate-mirrors config",
    about,
    version,
    rename_all = "kebab-case",
    rename_all_env = "SCREAMING_SNAKE_CASE"
)]
pub struct Config {
    /// Per-mirror speed test timeout in milliseconds
    #[command(subcommand)]
    pub target: Target,

    /// Test only specified protocols (can be passed multiple times)
    #[arg(env = "RATE_MIRRORS_PROTOCOL", long = "protocol", name = "protocol")]
    pub protocols: Vec<Protocol>,

    /// Per-mirror speed test timeout in milliseconds. It is doubled in cases where slow connection
    /// times are detected
    #[arg(env = "RATE_MIRRORS_PER_MIRROR_TIMEOUT", long, default_value = "8000")]
    pub per_mirror_timeout: u64,

    /// Minimum downloading time, required to measure mirror speed,
    /// in milliseconds
    #[arg(env = "RATE_MIRRORS_MIN_PER_MIRROR", long, default_value = "300")]
    pub min_per_mirror: u64,

    /// Maximum downloading time, required to measure mirror speed,
    /// in milliseconds
    #[arg(env = "RATE_MIRRORS_MAX_PER_MIRROR", long, default_value = "1000")]
    pub max_per_mirror: u64,

    /// Minimum number of bytes to be downloaded,
    /// required to measure mirror speed
    #[arg(
        env = "RATE_MIRRORS_MIN_BYTES_PER_MIRROR",
        long,
        default_value = "70000"
    )]
    pub min_bytes_per_mirror: usize,

    /// Per-mirror: sigma to mean speed ratio
    ///
    ///   1.0 -- 68% probability (1 sigma), no 100% error
    ///   0.5 -- 68% probability (1 sigma), no 50% error;
    ///   0.25 -- 68% probability (1 sigma), no 25% error;
    ///   0.125 -- 95% probability (2 sigmas), no 25% error;
    ///   0.0625 -- 95% probability (2 sigmas), no 12.5% error:
    #[arg(
        env = "RATE_MIRRORS_EPS",
        long,
        default_value = "0.0625",
        verbatim_doc_comment
    )]
    pub eps: f64,

    /// Per-mirror: after min measurement time elapsed, check such number of
    /// subsequently downloaded data chunks whether speed variations are less
    /// then "eps"
    #[arg(env = "RATE_MIRRORS_EPS_CHECKS", long, default_value = "40")]
    pub eps_checks: usize,

    /// Number of simultaneous speed tests
    #[arg(env = "RATE_MIRRORS_CONCURRENCY", long, default_value = "16")]
    pub concurrency: usize,

    /// Number of simultaneous speed tests for mirrors with unknown country
    #[arg(
        env = "RATE_MIRRORS_CONCURRENCY_FOR_UNLABELED",
        long,
        default_value = "40"
    )]
    pub concurrency_for_unlabeled: usize,

    /// Max number of jumps between countries, when finding top mirrors
    #[arg(env = "RATE_MIRRORS_MAX_JUMPS", long, default_value = "12")]
    pub max_jumps: usize,

    /// Entry country - first country (+ its neighbours) to test.
    /// You don't need to change it unless you are just curious.
    #[arg(
        env = "RATE_MIRRORS_ENTRY_COUNTRY",
        long,
        default_value = "US",
        verbatim_doc_comment
    )]
    pub entry_country: String,

    /// Neighbor country to test per country
    #[arg(
        env = "RATE_MIRRORS_COUNTRY_NEIGHBORS_PER_COUNTRY",
        long,
        default_value = "9"
    )]
    pub country_neighbors_per_country: usize,

    /// Number of mirrors to test per country
    #[arg(
        env = "RATE_MIRRORS_COUNTRY_TEST_MIRRORS_PER_COUNTRY",
        long,
        default_value = "21"
    )]
    pub country_test_mirrors_per_country: usize,

    /// Number of top mirrors to retest
    #[arg(
        env = "RATE_MIRRORS_TOP_MIRRORS_NUMBER_TO_RETEST",
        long,
        default_value = "42"
    )]
    pub top_mirrors_number_to_retest: usize,

    /// Max number of mirrors to output
    #[arg(env = "RATE_MIRRORS_MAX_MIRRORS_TO_OUTPUT", long)]
    pub max_mirrors_to_output: Option<usize>,

    /// Filename to save the output to in case of success
    #[arg(env = "RATE_MIRRORS_SAVE", long = "save", verbatim_doc_comment)]
    pub save_to_file: Option<String>,

    /// Allow running by root
    #[arg(env = "RATE_MIRRORS_ALLOW_ROOT", long)]
    pub allow_root: bool,

    /// Disable printing comments
    #[arg(env = "RATE_MIRRORS_DISABLE_COMMENTS", long)]
    pub disable_comments: bool,

    /// Disable printing comments to output file
    #[arg(env = "RATE_MIRRORS_DISABLE_COMMENTS_IN_FILE", long)]
    pub disable_comments_in_file: bool,

    /// Enable freshness with optional speed priority (0-1); bare flag uses 1
    #[arg(
        env = "RATE_MIRRORS_FRESHNESS_CHECK",
        long,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "1",
        default_value = "off"
    )]
    pub freshness_check: FreshnessMode,

    /// Path to local reference database directory
    #[arg(
        env = "RATE_MIRRORS_REF_LOCAL_DIR",
        long,
        default_value = "/var/lib/pacman/sync"
    )]
    pub ref_local_dir: String,

    /// Timeout for freshness check downloads in milliseconds
    #[arg(env = "RATE_MIRRORS_FRESHNESS_TIMEOUT", long, default_value = "15000")]
    pub freshness_timeout: u64,
}

fn parse_speed_weight(value: &str) -> Result<f64, String> {
    let weight = value
        .parse::<f64>()
        .map_err(|_| "speed weight must be a number from 0 to 1".to_string())?;
    if weight.is_finite() && (0.0..=1.0).contains(&weight) {
        Ok(weight)
    } else {
        Err("speed weight must be a number from 0 to 1".to_string())
    }
}

impl Config {
    pub fn is_protocol_allowed(&self, protocol: &Protocol) -> bool {
        self.protocols.is_empty() || self.protocols.contains(protocol)
    }

    pub fn is_protocol_allowed_for_url(&self, url: &Url) -> bool {
        self.protocols.is_empty()
            || url
                .scheme()
                .parse()
                .map(|p| self.protocols.contains(&p))
                .unwrap_or(false)
    }

    pub fn get_preferred_url<'a>(&self, urls: &'a [Url]) -> Option<&'a Url> {
        urls.iter()
            .filter(|u| self.is_protocol_allowed_for_url(u))
            .sorted_by_key(|u| match u.scheme() {
                "https" => 0,
                "http" => 1,
                _ => 2,
            })
            .next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freshness_is_opt_in_and_weight_has_defined_endpoints() {
        let default = Config::try_parse_from(["rate-mirrors", "arch"]).unwrap();
        assert_eq!(default.freshness_check, FreshnessMode::Off);

        let bare = Config::try_parse_from(["rate-mirrors", "--freshness-check", "arch"]).unwrap();
        assert_eq!(bare.freshness_check, FreshnessMode::On(1.0));
        let weighted =
            Config::try_parse_from(["rate-mirrors", "--freshness-check=0.9", "arch"]).unwrap();
        assert_eq!(weighted.freshness_check, FreshnessMode::On(0.9));
        let freshness_first =
            Config::try_parse_from(["rate-mirrors", "--freshness-check=0", "arch"]).unwrap();
        assert_eq!(freshness_first.freshness_check, FreshnessMode::On(0.0));
        let benchmark = Config::try_parse_from([
            "rate-mirrors",
            "--freshness-check=0.5",
            "--top-mirrors-number-to-retest=0",
            "arch",
        ])
        .unwrap();
        assert_eq!(benchmark.top_mirrors_number_to_retest, 0);
        assert_eq!(
            "true".parse::<FreshnessMode>().unwrap(),
            FreshnessMode::On(1.0)
        );
        assert_eq!(
            "false".parse::<FreshnessMode>().unwrap(),
            FreshnessMode::Off
        );
    }

    #[test]
    fn freshness_weight_rejects_out_of_range_and_nonfinite_values() {
        for value in ["-0.1", "1.1", "NaN", "inf", "oops"] {
            assert!(value.parse::<FreshnessMode>().is_err(), "{value}");
        }
    }
}
