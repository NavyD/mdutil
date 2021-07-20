use anyhow::{anyhow, Error, Result};
use mdutil::image::{Checker, LinkType};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use structopt::StructOpt;

#[derive(StructOpt, Debug)]
enum Opt {
    Check {
        #[structopt(flatten)]
        common: Common,
        #[structopt(flatten)]
        args: CheckerArgs,
    },
}

impl Opt {
    fn common(&self) -> &Common {
        match self {
            Opt::Check {
                common,
                args: _,
            } => common,
        }
    }
}

#[derive(Clone, Debug, StructOpt)]
pub struct CheckerArgs {
    #[structopt(long, parse(try_from_str = parse_duration_secs), default_value = "5")]
    pub connect_timeout: Duration,
    #[structopt(long, parse(try_from_str), default_value = "local")]
    pub image_link_type: LinkType,
}

fn parse_duration_secs(s: &str) -> Result<Duration> {
    s.parse::<u64>()
        .map(|v| Duration::from_secs(v))
        .map_err(Into::into)
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

#[tokio::main(flavor = "multi_thread")]
// #[tokio::main]
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
            args,
        } => {
            let checker = Checker::new(args);
            do_check(&paths, &checker, image_link_type).await;
        }
    }
    Ok(())
}

async fn do_check(paths: &[PathBuf], checker: &Checker, image_link_type: LinkType) {
    let tasks = paths
        .iter()
        .map(|path| {
            let checker = checker.clone();
            let path = path.to_owned();
            tokio::spawn(async move {
                let infos = checker.check(&path, image_link_type).await?;
                let err_infos = infos
                    .iter()
                    .filter(|info| info.error.is_some())
                    .collect::<Vec<_>>();
                if err_infos.is_empty() {
                    return Ok(());
                }
                let text = format!(
                    "`{}` has {} problems:\n",
                    path.canonicalize()?.to_str().unwrap(),
                    err_infos.len()
                );
                let err_text = err_infos.iter().fold(text, |mut acc, info| {
                    acc += &format!(
                        "row: {}, link: {}, error: {}, line: {}\n",
                        info.row_num,
                        info.link,
                        info.error.as_ref().unwrap(),
                        info.line
                    );
                    acc
                });
                println!("{}", err_text);
                Ok::<_, Error>(())
            })
        })
        .collect::<Vec<_>>();
    let tasks = futures::future::join_all(tasks).await;
    println!("check tasks {} completed", tasks.len());
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
