use crate::countries::Country;
use std::fmt;
use url::Url;

#[derive(Debug)]
pub struct MirrorInfo {
    pub url: Url,
    pub country: Option<&'static Country>,
}

impl MirrorInfo {
    pub fn new(url: Url, country: Option<&str>) -> Self {
        let country = country.and_then(Country::from_str);
        Self { url, country }
    }

    pub fn parse(input: &str, sep: &str) -> Result<Self, MirrorParseError> {
        let args = input.trim().split(sep).collect::<Vec<_>>();

        match args.len() {
            1 => match Url::parse(args[0]) {
                Ok(url) => Ok(MirrorInfo::new(url, None)),
                Err(_) => Err(MirrorParseError::BadUrl(args[0].to_string())),
            },
            2 => match Url::parse(args[0]) {
                Ok(url) => Ok(MirrorInfo::new(url, Some(args[1]))),
                Err(_) => match Url::parse(args[1]) {
                    Ok(url) => Ok(MirrorInfo::new(url, Some(args[0]))),
                    Err(_) => Err(MirrorParseError::BadUrl(args[1].to_string())),
                },
            },
            _ => Err(MirrorParseError::BadLine(input.to_string())),
        }
    }
}

impl fmt::Display for MirrorInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.country {
            Some(country) => {
                write!(f, "[{}] {}", country.code, &self.url)
            }
            None => {
                write!(f, "{}", &self.url)
            }
        }
    }
}

#[derive(Debug)]
pub enum MirrorParseError {
    BadLine(String),
    BadUrl(String),
}

impl fmt::Display for MirrorParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MirrorParseError::BadLine(s) => write!(f, "Bad line: {}", s),
            MirrorParseError::BadUrl(s) => write!(f, "Bad url: {}", s),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Mirror {
    pub url: Url,
    pub url_to_test: Url,
    pub country: Option<&'static Country>,
}

impl Mirror {
    /// Repository path shared by a `.files` speed probe and its `.db` database.
    pub fn repository_base_path(&self) -> Option<String> {
        self.url_to_test
            .path()
            .strip_suffix(".files")
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
    }

    pub fn database_url(&self) -> Option<Url> {
        let mut url = self.url_to_test.clone();
        url.set_path(&format!("{}.db", self.repository_base_path()?));
        url.set_query(None);
        url.set_fragment(None);
        Some(url)
    }
}

#[cfg(test)]
mod freshness_path_tests {
    use super::*;

    #[test]
    fn derives_database_from_normal_probe() {
        let mirror = Mirror {
            url: Url::parse("https://mirror.example/arch/").unwrap(),
            url_to_test: Url::parse("https://mirror.example/arch/extra/os/x86_64/extra.files")
                .unwrap(),
            country: None,
        };
        assert_eq!(
            mirror.repository_base_path().as_deref(),
            Some("/arch/extra/os/x86_64/extra")
        );
        assert_eq!(
            mirror.database_url().unwrap().as_str(),
            "https://mirror.example/arch/extra/os/x86_64/extra.db"
        );
    }

    #[test]
    fn non_database_probe_has_no_database_url() {
        let mirror = Mirror {
            url: Url::parse("https://mirror.example/").unwrap(),
            url_to_test: Url::parse("https://mirror.example/test.iso").unwrap(),
            country: None,
        };
        assert!(mirror.database_url().is_none());
    }

    #[test]
    fn encoded_path_does_not_change_when_building_database_url() {
        let mirror = Mirror {
            url: Url::parse("https://mirror.example/repo%20name/").unwrap(),
            url_to_test: Url::parse("https://mirror.example/repo%20name/extra.files").unwrap(),
            country: None,
        };
        assert_eq!(
            mirror.database_url().unwrap().as_str(),
            "https://mirror.example/repo%20name/extra.db"
        );
    }
}
