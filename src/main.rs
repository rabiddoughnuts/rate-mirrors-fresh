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
use clap::Parser;
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
            },
            None => Self {
                formatter,
                filename: None,
                output_lines: None,
                comments_enabled,
                comments_in_file_enabled,
                pacman_mirrorlist,
            },
        };
        Ok(output)
    }

    pub fn display_comment(&mut self, line: impl Display) {
        if self.comments_enabled {
            // Parser and network errors may contain newlines. Every physical
            // line must remain a comment in a saved pacman mirrorlist.
            for physical_line in line.to_string().split('\n') {
                let s = self
                    .formatter
                    .format_comment(physical_line.trim_end_matches('\r'));
                println!("{}", &s);
                if self.comments_in_file_enabled {
                    if let Some(output_lines) = &mut self.output_lines {
                        output_lines.push(s);
                    }
                }
            }
        }
    }

    pub fn display_mirror(&mut self, mirror: &Mirror) {
        let s = self.formatter.format_mirror(&mirror);
        println!("{}", &s);
        if let Some(output_lines) = &mut self.output_lines {
            output_lines.push(s);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
        output.display_comment("[WARN] bad archive\n<html>\r\n<head>");
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
}

fn main() -> Result<(), AppError> {
    let config = Arc::new(Config::parse());
    if !config.allow_root && Uid::effective().is_root() {
        return Err(AppError::Root);
    }
    let max_mirrors_to_output = config.max_mirrors_to_output.clone();
    let pacman_mirrorlist = !matches!(&config.target, Target::Stdin(_) | Target::OpenBSD(_));
    let require_verified_freshness = config.freshness_check.is_enabled()
        && !matches!(
            &config.target,
            Target::Stdin(_) | Target::OpenBSD(_) | Target::ArcoLinux(_)
        );

    let ref formatter = Arc::clone(&config).target;
    let mut output = OutputSink::new(
        formatter,
        config.save_to_file.as_deref(),
        !config.disable_comments,
        !config.disable_comments_in_file,
        pacman_mirrorlist,
    )?;

    output.display_comment(format!("STARTED AT: {}", Local::now()));
    output.display_comment(format!("ARGS: {}", env::args().join(" ")));

    let (tx_progress, rx_progress) = mpsc::channel::<String>();
    let (tx_results, rx_results) = mpsc::channel::<SpeedTestResults>();
    let (tx_mirrors, rx_mirrors) = mpsc::channel::<Mirror>();

    let thread_handle = thread::spawn(move || -> Result<(), AppError> {
        let mirrors = config
            .target
            .fetch_mirrors(Arc::clone(&config), tx_progress.clone())?;

        // sending untested mirrors back so we have a fallback in case if all tests fail
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
        output.display_comment(progress);
    }

    thread_handle.join().unwrap()?;

    let results: Vec<_> = rx_results.iter().flatten().collect();

    if results.is_empty() {
        if require_verified_freshness {
            return Err(AppError::NoFreshMirrorsVerified);
        }
        let untested_mirrors: Vec<Mirror> = rx_mirrors.into_iter().collect();
        if untested_mirrors.len() == 0 {
            output.display_comment("==== NO MIRRORS AFTER FILTERING ====");
            return Err(AppError::NoMirrorsAfterFiltering);
        }
        output.display_comment("==== FAILED TO TEST SPEEDS, RETURNING UNTESTED MIRRORS ====");
        for mirror in untested_mirrors.into_iter() {
            output.display_mirror(&mirror);
        }
    } else {
        output.display_comment("==== RESULTS (ranked mirrors) ====");

        for (index, result) in results.iter().enumerate() {
            output.display_comment(format!("{:>3}. {}", index + 1, result));
        }

        output.display_comment(format!("FINISHED AT: {}", Local::now()));

        let it: Box<dyn Iterator<Item = SpeedTestResult>> = match max_mirrors_to_output {
            Some(n) => Box::new(results.into_iter().take(n)),
            None => Box::new(results.into_iter()),
        };

        for result in it {
            output.display_mirror(&result.item);
        }
    }

    output.save_to_file()?;
    Ok(())
}
