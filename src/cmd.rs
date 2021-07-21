use crate::image::{Checker, Link, LinkType};
use anyhow::{anyhow, Error, Result};
use futures::future;
use log::{error, info, warn};
use std::collections::HashSet;
use std::fs;
use std::fs::{create_dir, create_dir_all};
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
    Replace {
        #[structopt(flatten)]
        common: Common,
        #[structopt(flatten)]
        args: CheckerArgs,
        #[structopt(short, long, parse(from_os_str))]
        output: PathBuf,
    },
}

impl Opt {
    fn common(&self) -> &Common {
        match self {
            Opt::Check { common, args: _ } => common,
            Opt::Replace {
                common,
                args: _,
                output: _,
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
        .map(Duration::from_secs)
        .map_err(Into::into)
}

#[derive(StructOpt, Debug, Default, Clone)]
struct Common {
    /// log level. default error
    #[structopt(short = "v", parse(from_occurrences))]
    verbose: u8,

    /// Input files
    #[structopt(parse(from_os_str))]
    input: Vec<PathBuf>,

    /// use `,` limit the markdown docs. like `--extensions md,markdown,mk`.
    /// default md
    #[structopt(short, long, parse(from_str = parse_extensions), default_value="md,markdown")]
    extensions: HashSet<String>,

    #[structopt(short, long)]
    recursive: bool,
}

fn parse_extensions(s: &str) -> HashSet<String> {
    s.split(',')
        .map(|s| s.trim().to_string())
        .collect::<HashSet<String>>()
}

pub async fn run() -> Result<()> {
    let opt = Opt::from_args();
    init_log(opt.common().verbose)?;

    let paths = opt.common().load_input_paths()?;
    match opt {
        Opt::Check { common: _, args } => {
            let checker = Checker::new(args);
            do_check(&paths, &checker).await;
        }
        Opt::Replace { args, output, .. } => do_replace(&paths, &output, args).await,
    }
    Ok(())
}

async fn do_replace(inputs: &[PathBuf], output: &Path, args: CheckerArgs) {
    if matches!(args.image_link_type, LinkType::None) {
        println!("stopping replace action with args: image_link_type=None");
        return;
    }
    if output.exists() {
        if fs::read_dir(&output).map_or(true, |mut dir| dir.next().is_some()) {
            eprintln!("non empty out dir: {:?}", output);
            return;
        }
    } else {
        info!("creating out dir: {:?}", output);
        create_dir(&output).unwrap_or_else(|e| panic!("create dir {:?} error: {}", output, e));
    }
    let output = output.canonicalize().unwrap();

    let checker = Checker::new(args);
    let jobs = inputs
        .iter()
        .filter(|path| {
            let parent = output.join(&path);
            let parent = parent.parent().unwrap();
            if parent.exists() {
                return true;
            }
            if let Err(e) = create_dir_all(&parent) {
                error!("create out dir {:?} error: {}", parent, e);
                false
            } else {
                true
            }
        })
        .map(|p| {
            let checker = checker.clone();
            let path = p.to_path_buf();
            let out_path = output.clone().join(&path);
            tokio::spawn(async move {
                let (text, errs) = checker.embed_base64(&path).await?;
                let mut out_text = "".to_string();
                if !errs.is_empty() {
                    out_text += &format!("found {} errors in {:?}\n", errs.len(), path);
                    out_text = errs.iter().fold(out_text, |mut acc, e| {
                        acc += &format!(
                            "link: {}, cause: {}\n",
                            e.downcast_ref::<Link>().unwrap(),
                            e.root_cause()
                        );
                        acc
                    });
                }
                info!(
                    "writing replacement result to path {:?} from {:?}",
                    out_path, path
                );
                out_text += "\n";
                if let Err(e) = fs::write(&out_path, text) {
                    out_text += &format!("\nwrite {:?} failed: {}\n", out_path, e);
                    eprintln!("{}", out_text);
                } else {
                    println!("{}", out_text);
                }
                Ok::<_, Error>(())
            })
        })
        .collect::<Vec<_>>();

    for job in future::join_all(jobs).await {
        if let Err(e) = job.map_err(Into::into).and_then(|e| e) {
            eprintln!("async task failed: {}", e);
        }
    }
    println!("all replace tasks completed");
}

async fn do_check(paths: &[PathBuf], checker: &Checker) {
    let tasks = paths
        .iter()
        .map(|path| {
            let checker = checker.clone();
            let path = path.to_owned();
            tokio::spawn(async move {
                let infos = checker.check(&path).await?;
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
        info!("loading all paths from {:?}", self.input);
        if self.input.is_empty() {
            return Err(anyhow!("empty input paths"));
        }
        // distinct input paths
        let filtered_input_paths = self
            .input
            .iter()
            .map(|p| p.canonicalize())
            .filter(|p| match p {
                Ok(_) => true,
                Err(e) => {
                    error!("{:?} canonicalize failed: {}", p, e);
                    false
                }
            })
            .map(Result::unwrap)
            .collect::<HashSet<_>>();

        if filtered_input_paths.len() < self.input.len() {
            warn!(
                "found duplicate in input paths: {:?}. filtered paths: {:?}",
                self.input, filtered_input_paths
            );
        }

        let mut distinct = HashSet::new();
        let mut paths = vec![];
        for path in &self.input {
            for subpath in self.load_paths(&path)? {
                let p = subpath.canonicalize()?;
                let s = p.to_str().unwrap().to_string();
                if !distinct.insert(p) {
                    info!("duplicative path {} in top path: {:?}", s, path);
                    continue;
                }
                paths.push(subpath);
            }
        }
        Ok(paths)
        // scan all distinct paths
        // let mut paths = HashSet::new();
        // for path in filtered_input_paths {
        //     for p in self.load_paths(&path)? {
        //         let p = p.canonicalize()?;
        //         let s = p.to_str().unwrap().to_string();
        //         if !paths.insert(p) {
        //             info!("duplicative path {} in top path: {:?}", s, path);
        //         }
        //     }
        // }
        // info!(
        //     "loaded {} markdown files in {} top paths",
        //     paths.len(),
        //     self.input.len()
        // );
        // Ok(paths.into_iter().collect())
    }

    fn load_paths(&self, path: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
        let path = path.as_ref();
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

    // fn load(path: &Path) -> Result<Vec<PathBuf>> {
    //     let a = fs::read_dir(path)?
    //         .collect::<Vec<_>>()
    //         .par_iter()
    //         .filter(|entry| {
    //             if let Err(e) = entry {
    //                 warn!("read dir {:?} failed: {}", path, e);
    //                 false
    //             } else {
    //                 true
    //             }
    //         })
    //         .map(|res| res.as_ref().unwrap())
    //         ;
    //     todo!()
    // }

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

// #[cfg(test)]
// mod tests {
//     use super::*;
//     use crate::tests::DATA_DIR;
//     use once_cell::sync::Lazy;

//     static COM: Lazy<Common> = Lazy::new(|| Common {
//         input: DATA_DIR.to_path_buf(),
//         extensions: {
//             let mut set = HashSet::new();
//             set.insert("md".to_string());
//             set.insert("markdown".to_string());
//             set
//         },
//         ..Default::default()
//     });

//     #[test]
//     fn test_load_input_paths() -> Result<()> {
//         let mut com = COM.clone();
//         com.recursive = true;
//         let paths = com.load_input_paths()?;
//         assert_eq!(paths.len(), 4);

//         let mut com = COM.clone();
//         com.recursive = false;
//         let paths = com.load_input_paths()?;
//         assert_eq!(paths.len(), 1);

//         let mut com = COM.clone();
//         com.input = format!("{}/testa.md", DATA_DIR).parse()?;
//         com.recursive = false;
//         let paths = com.load_input_paths()?;
//         assert_eq!(paths.len(), 1);

//         let mut com = COM.clone();
//         com.input = format!("{}/test.png", DATA_DIR).parse()?;
//         assert!(com.load_input_paths().is_err());
//         Ok(())
//     }

//     #[test]
//     fn test_dir_recursive() -> Result<()> {
//         let com = COM.clone();
//         let mut paths = vec![];
//         com.visite_recursive(&com.input, &mut paths)?;

//         assert_eq!(paths.len(), 4);
//         assert!(paths.iter().all(|path| com.is_markdown_file(path)));
//         Ok(())
//     }

//     #[test]
//     fn test_is_markdown() -> Result<()> {
//         let com = &COM;
//         assert!(com.is_markdown_file(&format!("{}/testa.md", DATA_DIR)));
//         assert!(com.is_markdown_file(&format!("{}/a/another.markdown", DATA_DIR)));

//         assert!(!com.is_markdown_file(&format!("{}/a/non-markdown.nonmd", DATA_DIR)));
//         assert!(!com.is_markdown_file(&format!("{}/test.png", DATA_DIR)));
//         Ok(())
//     }
// }
