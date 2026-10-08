#[macro_use]
extern crate lazy_static;

mod config;
mod countries;
mod freshness;
mod mirror;
mod speed_test;
mod target_configs;
mod targets;

use crate::config::{AppError, Config, FetchMirrors, Target};
use crate::speed_test::{SpeedTestResult, SpeedTestResults, test_speed_by_countries};
use chrono::prelude::*;
use config::LogFormatter;
use itertools::Itertools;
use mirror::Mirror;
use nix::unistd::Uid;
use std::env;
use std::fmt::Display;
use std::fs::File;
use std::io;
use std::io::prelude::*;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;

struct OutputSink<'a, T: LogFormatter> {
    filename: Option<String>,
    output_lines: Option<Vec<String>>,
    formatter: &'a T,
    comments_enabled: bool,
    comments_in_file_enabled: bool,
    pacman_mirrorlist: bool,
    mirror_count: usize,
}

impl<'a, T: LogFormatter> OutputSink<'a, T> {
    pub fn new(
        formatter: &'a T,
        filename: Option<&str>,
        comments_enabled: bool,
        comments_in_file_enabled: bool,
        pacman_mirrorlist: bool,
    ) -> Result<Self, io::Error> {
        let output = match filename {
            Some(filename) => Self {
                formatter,
                filename: Some(filename.to_string()),
                output_lines: Some(Vec::new()),
                comments_enabled,
                comments_in_file_enabled,
                pacman_mirrorlist,
                mirror_count: 0,
            },
            None => Self {
                formatter,
                filename: None,
                output_lines: None,
                comments_enabled,
                comments_in_file_enabled,
                pacman_mirrorlist,
                mirror_count: 0,
            },
        };
        Ok(output)
    }

    fn write_stdout_line(&mut self, line: &str) -> Result<(), AppError> {
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "{}", line).map_err(|err| {
            if err.kind() == io::ErrorKind::BrokenPipe {
                AppError::StdoutBrokenPipe
            } else {
                AppError::IoError(err)
            }
        })
    }

    pub fn display_comment(&mut self, line: impl Display) -> Result<(), AppError> {
        if self.comments_enabled {
            // Parser and network errors may contain newlines. Every physical
            // line must remain a comment in a saved pacman mirrorlist.
            for physical_line in line.to_string().split('\n') {
                let s = self
                    .formatter
                    .format_comment(physical_line.trim_end_matches('\r'));
                self.write_stdout_line(&s)?;
                if self.comments_in_file_enabled {
                    if let Some(output_lines) = &mut self.output_lines {
                        output_lines.push(s);
                    }
                }
            }
        }
        Ok(())
    }

    pub fn display_mirror(&mut self, mirror: &Mirror) -> Result<(), AppError> {
        let s = self.formatter.format_mirror(&mirror);
        self.write_stdout_line(&s)?;
        if let Some(output_lines) = &mut self.output_lines {
            output_lines.push(s);
        }
        self.mirror_count += 1;
        Ok(())
    }

    pub fn save_to_file(&mut self) -> Result<(), io::Error> {
        if let Some(output_lines) = &mut self.output_lines {
            if let Some(filename) = self.filename.as_ref() {
                if self.pacman_mirrorlist
                    && (!output_lines
                        .iter()
                        .any(|line| line.starts_with("Server = "))
                        || output_lines.iter().any(|line| !valid_pacman_line(line)))
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "refusing to save an invalid pacman mirrorlist",
                    ));
                }
                let mut f = File::create(filename)?;
                f.write_all(output_lines.join("\n").as_bytes())?;
                f.write_all("\n".as_bytes())?;
            }
        }
        return Ok(());
    }
}

fn valid_pacman_line(line: &str) -> bool {
    if line.chars().any(|ch| ch.is_control()) {
        return false;
    }
    if line.is_empty() || line.starts_with('#') {
        return true;
    }
    let Some(url) = line.strip_prefix("Server = ") else {
        return false;
    };
    (url.starts_with("https://") || url.starts_with("http://"))
        && !url.chars().any(|ch| ch.is_whitespace())
}

fn apply_base_path_override(mirrors: &mut [Mirror], base_path: &str) -> Result<(), AppError> {
    for mirror in mirrors {
        mirror.url_to_test = mirror.url.join(&format!("{}.files", base_path))?;
    }
    Ok(())
}

fn probe_base_path(target: &Target, base_path: &str) -> String {
    match target {
        Target::Manjaro(manjaro) => format!("{}/{}", manjaro.branch, base_path),
        _ => base_path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    struct TestFormatter;

    impl LogFormatter for TestFormatter {
        fn format_comment(&self, message: impl Display) -> String {
            format!("# {}", message)
        }

        fn format_mirror(&self, _mirror: &Mirror) -> String {
            unreachable!()
        }
    }

    #[test]
    fn multiline_diagnostic_stays_commented_in_saved_output() {
        let mut output = OutputSink::new(&TestFormatter, Some("unused"), true, true, true).unwrap();
        output
            .display_comment("[WARN] bad archive\n<html>\r\n<head>")
            .unwrap();
        assert_eq!(
            output.output_lines.unwrap(),
            vec!["# [WARN] bad archive", "# <html>", "# <head>"]
        );
    }

    #[test]
    fn pacman_output_rejects_raw_html_and_requires_a_server() {
        assert!(!valid_pacman_line("<html lang=\"ko\">"));
        assert!(!valid_pacman_line(
            "Server = https://example.org/\nInclude = /tmp/other"
        ));
        assert!(valid_pacman_line("# <html>"));
        assert!(valid_pacman_line(
            "Server = https://example.org/$repo/os/$arch"
        ));

        let mut output = OutputSink::new(&TestFormatter, Some("unused"), true, true, true).unwrap();
        output.output_lines = Some(vec!["# diagnostic".to_string(), "<html>".to_string()]);
        assert_eq!(
            output.save_to_file().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        output.output_lines = Some(vec!["# no server".to_string()]);
        assert_eq!(
            output.save_to_file().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn base_path_override_updates_both_probe_urls() {
        let mut mirrors = vec![Mirror {
            url: url::Url::parse("https://mirror.example/repo/").unwrap(),
            url_to_test: url::Url::parse("https://mirror.example/repo/old.files").unwrap(),
            country: None,
        }];
        apply_base_path_override(&mut mirrors, "x86_64/cachyos/cachyos").unwrap();
        assert_eq!(
            mirrors[0].url_to_test.as_str(),
            "https://mirror.example/repo/x86_64/cachyos/cachyos.files"
        );
        assert_eq!(
            mirrors[0].database_url().unwrap().as_str(),
            "https://mirror.example/repo/x86_64/cachyos/cachyos.db"
        );
    }

    #[test]
    fn manjaro_override_keeps_the_selected_branch() {
        let config =
            Config::try_parse_from(["rate-mirrors", "manjaro", "--branch", "testing"]).unwrap();
        assert_eq!(
            probe_base_path(&config.target, "extra/x86_64/extra"),
            "testing/extra/x86_64/extra"
        );
    }
}

fn main() -> Result<(), AppError> {
    match run() {
        Err(AppError::StdoutBrokenPipe) => Ok(()),
        result => result,
    }
}

fn run() -> Result<(), AppError> {
    let config = Arc::new(Config::new());
    if !config.allow_root && Uid::effective().is_root() {
        return Err(AppError::Root);
    }
    let max_mirrors_to_output = config.max_mirrors_to_output.clone();
    let pacman_mirrorlist = !matches!(&config.target, Target::Stdin(_) | Target::OpenBSD(_));
    let require_verified_freshness =
        config.freshness_check.is_enabled() && config.target.supports_freshness();
    let disable_untested_fallback = config.disable_untested_fallback;

    let ref formatter = Arc::clone(&config).target;
    let mut output = OutputSink::new(
        formatter,
        config.save_to_file.as_deref(),
        !config.disable_comments,
        !config.disable_comments_in_file,
        pacman_mirrorlist,
    )?;

    output.display_comment(format!("STARTED AT: {}", Local::now()))?;
    output.display_comment(format!("VERSION: {}", env!("CARGO_PKG_VERSION")))?;
    output.display_comment(format!("ARGS: {}", env::args().join(" ")))?;

    let (tx_progress, rx_progress) = mpsc::channel::<String>();
    let (tx_results, rx_results) = mpsc::channel::<SpeedTestResults>();
    let (tx_mirrors, rx_mirrors) = mpsc::channel::<Mirror>();

    let thread_handle = thread::spawn(move || -> Result<(), AppError> {
        let mut mirrors = config.target.fetch_mirrors(tx_progress.clone())?;

        // Keep upstream target-specific .files paths by default. A global
        // --base-path override retains this fork's repository-path option.
        if let Some(base_path) = &config.base_path {
            if config.target.supports_freshness() {
                apply_base_path_override(
                    &mut mirrors,
                    &probe_base_path(&config.target, base_path),
                )?;
            }
        }

        // Centralized protocol filtering
        let before_protocol = mirrors.len();
        mirrors.retain(|m| config.is_protocol_allowed_for_url(&m.url));
        if mirrors.len() < before_protocol {
            tx_progress
                .send(format!(
                    "PROTOCOL FILTER: {} -> {} mirrors",
                    before_protocol,
                    mirrors.len()
                ))
                .unwrap();
        }

        // Country filtering before dedup so excluded-country duplicates
        // don't shadow valid mirrors from non-excluded countries
        let before_country = mirrors.len();
        mirrors.retain(|m| !config.is_country_excluded(m.country.map(|c| c.code).unwrap_or("zz")));
        if mirrors.len() < before_country {
            tx_progress
                .send(format!(
                    "COUNTRY FILTER: {} -> {} mirrors",
                    before_country,
                    mirrors.len()
                ))
                .unwrap();
        }

        // Prefer https over http when both are available for the same host
        mirrors.sort_by_key(|m| match m.url.scheme() {
            "https" => 0,
            "http" => 1,
            _ => 2,
        });

        // Deduplicate mirrors by host+port+path (keeps first = preferred protocol)
        let before_dedup = mirrors.len();
        let mut seen = std::collections::HashSet::new();
        mirrors.retain(|m| {
            let key = format!(
                "{}{}{}",
                m.url.host_str().unwrap_or(""),
                m.url.port().map(|p| format!(":{}", p)).unwrap_or_default(),
                m.url.path()
            );
            seen.insert(key)
        });
        if mirrors.len() < before_dedup {
            tx_progress
                .send(format!(
                    "DEDUP: {} -> {} mirrors",
                    before_dedup,
                    mirrors.len()
                ))
                .unwrap();
        }

        // sending filtered mirrors back so we have a fallback in case if all tests fail
        for mirror in mirrors.iter().cloned() {
            tx_mirrors.send(mirror).unwrap();
        }

        tx_progress
            .send(format!("MIRRORS LEFT AFTER FILTERING: {}", mirrors.len()))
            .unwrap();

        test_speed_by_countries(mirrors, config, tx_progress, tx_results);
        Ok(())
    });

    for progress in rx_progress.into_iter() {
        output.display_comment(progress)?;
    }

    thread_handle.join().unwrap()?;

    let results: Vec<_> = rx_results.iter().flatten().collect();

    if results.is_empty() {
        if require_verified_freshness {
            return Err(AppError::NoFreshMirrorsVerified);
        }
        let untested_mirrors: Vec<Mirror> = rx_mirrors.into_iter().collect();
        if untested_mirrors.len() == 0 {
            output.display_comment("==== NO MIRRORS AFTER FILTERING ====")?;
            return Err(AppError::NoMirrorsAfterFiltering);
        }
        if disable_untested_fallback {
            output.display_comment("==== ALL SPEED TESTS FAILED ====")?;
            return Err(AppError::SpeedTestsFailed);
        }
        output.display_comment("==== FAILED TO TEST SPEEDS, RETURNING UNTESTED MIRRORS ====")?;
        for mirror in untested_mirrors.into_iter() {
            output.display_mirror(&mirror)?;
        }
    } else {
        output.display_comment("==== RESULTS (ranked mirrors) ====")?;

        for (index, result) in results.iter().enumerate() {
            output.display_comment(format!("{:>3}. {}", index + 1, result))?;
        }

        output.display_comment(format!("FINISHED AT: {}", Local::now()))?;

        let it: Box<dyn Iterator<Item = SpeedTestResult>> = match max_mirrors_to_output {
            Some(n) => Box::new(results.into_iter().take(n)),
            None => Box::new(results.into_iter()),
        };

        for result in it {
            output.display_mirror(&result.item)?;
        }
    }

    if output.mirror_count == 0 {
        return Err(AppError::BlankOutput);
    }
    output.save_to_file()?;
    Ok(())
}
