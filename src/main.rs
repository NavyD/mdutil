use anyhow::{anyhow, Result};
use mdutil::image::{self, Converter, LinkType};
use std::collections::HashSet;
use std::fs;
use std::{
    convert::TryFrom,
    path::{Path, PathBuf},
};
use structopt::StructOpt;

#[derive(StructOpt, Debug)]
enum Opt {
    Check {
        #[structopt(flatten)]
        common: Common,
        #[structopt(long, parse(try_from_str), default_value = "All")]
        image_link_type: LinkType,
    },
}

impl Opt {
    fn common(&self) -> &Common {
        match self {
            Opt::Check {
                common,
                image_link_type: _,
            } => common,
            _ => {
                panic!()
            }
        }
    }
}

#[derive(StructOpt, Debug, Default, Clone)]
struct Common {
    /// log level. default error
    #[structopt(short = "v", parse(from_occurrences))]
    verbose: u8,

    /// Input files
    #[structopt(parse(from_os_str))]
    input: PathBuf,

    /// use `,` limit the markdown docs. like `--extensions md,markdown,mk`.
    /// default md
    #[structopt(parse(from_str = parse_extensions), default_value="md,markdown")]
    extensions: HashSet<String>,

    #[structopt(short)]
    recursive: bool,
}

fn parse_extensions(s: &str) -> HashSet<String> {
    s.split(',')
        .map(|s| s.trim().to_string())
        .collect::<HashSet<String>>()
}

#[tokio::main]
async fn main() -> Result<()> {
    let opt = Opt::from_args();
    init_log(opt.common().verbose)?;

    let paths = opt.common().load_input_paths()?;
    log::debug!("loaded markdown paths size: {}", paths.len());
    if paths.is_empty() {
        return Err(anyhow!("empty markdown paths"));
    }

    match opt {
        Opt::Check {
            common: _,
            image_link_type,
        } => {
            let con = Converter::new();
            for p in &paths {
                con.check(p, image_link_type).await?
            }
        }
    }
    Ok(())
}

fn init_log(verbose: u8) -> Result<()> {
    if verbose > 4 {
        return Err(anyhow!("invalid arg: 4 < {} number of verbose", verbose));
    }
    let level: log::LevelFilter = unsafe { std::mem::transmute((verbose + 1) as usize) };
    env_logger::builder()
        .filter_level(log::LevelFilter::Error)
        .filter_module(module_path!(), level)
        .init();
    Ok(())
}

impl Common {
    fn load_input_paths(&self) -> Result<Vec<PathBuf>> {
        let path = &self.input;
        log::trace!("loading from {:?}", path);
        if !path.exists() {
            return Err(anyhow!("{:?} does not exist", path.to_str()));
        }

        if self.recursive {
            if !path.is_dir() {
                return Err(anyhow!("{:?} is not dir", path.to_str()));
            }
            let mut paths = vec![];
            return self.visite_recursive(path, &mut paths).map(|_| paths);
        }

        if path.is_file() {
            if self.is_markdown_file(path) {
                return Ok(vec![path.to_path_buf()]);
            } else {
                return Err(anyhow!(
                    "{:?} is not markdown file: {:?}",
                    path,
                    self.extensions
                ));
            }
        }

        Ok(path
            .read_dir()?
            .filter(Result::is_ok)
            .map(Result::unwrap)
            .map(|dir| dir.path())
            .filter(|p| self.is_markdown_file(p))
            .collect())
    }

    fn is_markdown_file(&self, path: impl AsRef<Path>) -> bool {
        let path = path.as_ref();
        path.is_file()
            && path
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| self.extensions.contains(s))
                .unwrap_or(false)
    }

    fn visite_recursive(&self, path: impl AsRef<Path>, paths: &mut Vec<PathBuf>) -> Result<()> {
        if !path.as_ref().is_dir() {
            return Ok(());
        }

        for entry in fs::read_dir(path)? {
            let path = entry?.path();
            if path.is_dir() {
                self.visite_recursive(path, paths)?;
            } else if self.is_markdown_file(&path) {
                log::trace!("found {:?}", path);
                paths.push(path.to_path_buf());
            } else {
                log::trace!("ignore un md file: {:?}", path);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use once_cell::sync::Lazy;

    use super::*;

    const DATA_DIR: &str = "tests/resources";

    static COM: Lazy<Common> = Lazy::new(|| Common {
        input: DATA_DIR.parse().unwrap(),
        extensions: {
            let mut set = HashSet::new();
            set.insert("md".to_string());
            set.insert("markdown".to_string());
            set
        },
        ..Default::default()
    });

    #[test]
    fn test_check() -> Result<()> {
        let opt = Opt::Check {
            image_link_type: LinkType::All,
            common: COM.clone(),
        };

        Ok(())
    }

    #[test]
    fn test_load_input_paths() -> Result<()> {
        let mut com = COM.clone();
        com.recursive = true;
        let paths = com.load_input_paths()?;
        assert_eq!(paths.len(), 4);

        let mut com = COM.clone();
        com.recursive = false;
        let paths = com.load_input_paths()?;
        assert_eq!(paths.len(), 1);

        let mut com = COM.clone();
        com.input = format!("{}/testa.md", DATA_DIR).parse()?;
        com.recursive = false;
        let paths = com.load_input_paths()?;
        assert_eq!(paths.len(), 1);

        let mut com = COM.clone();
        com.input = format!("{}/test.png", DATA_DIR).parse()?;
        assert!(com.load_input_paths().is_err());
        Ok(())
    }

    #[test]
    fn test_dir_recursive() -> Result<()> {
        let com = COM.clone();
        let mut paths = vec![];
        com.visite_recursive(&com.input, &mut paths)?;

        assert_eq!(paths.len(), 4);
        assert!(paths.iter().all(|path| com.is_markdown_file(path)));
        Ok(())
    }

    #[test]
    fn test_is_markdown() -> Result<()> {
        let com = &COM;
        assert!(com.is_markdown_file(&format!("{}/testa.md", DATA_DIR)));
        assert!(com.is_markdown_file(&format!("{}/a/another.markdown", DATA_DIR)));

        assert!(!com.is_markdown_file(&format!("{}/a/non-markdown.nonmd", DATA_DIR)));
        assert!(!com.is_markdown_file(&format!("{}/test.png", DATA_DIR)));
        Ok(())
    }
}
