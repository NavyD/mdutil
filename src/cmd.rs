use crate::image::{Checker, Link, LinkType};
use crate::CRATE_NAME;
use anyhow::{anyhow, bail, Error, Result};
use futures::future;
use log::{debug, info, trace, warn};
use std::collections::HashSet;
use std::fmt::Debug;
use std::fs;
use std::fs::{create_dir, create_dir_all};
use std::io::stdout;
use std::path::{Path, PathBuf};
use std::time::Duration;
use structopt::StructOpt;
use tokio::fs::write;

#[derive(StructOpt, Debug)]
enum Opt {
    Check {
        #[structopt(flatten)]
        common: Common,
        #[structopt(flatten)]
        checker_args: CheckerArgs,
    },
    Replace {
        #[structopt(flatten)]
        common: Common,
        #[structopt(flatten)]
        checker_args: CheckerArgs,
        #[structopt(flatten)]
        replace_args: ReplaceArgs,
    },

    Completion,
}

impl Opt {
    fn common(&self) -> Option<&Common> {
        match self {
            Opt::Check { common, .. } => Some(common),
            Opt::Replace { common, .. } => Some(common),
            Opt::Completion => None,
        }
    }
}

#[derive(Clone, Debug, StructOpt)]
pub struct CheckerArgs {
    #[structopt(long, parse(try_from_str = parse_duration_secs), default_value = "5")]
    pub connect_timeout: Duration,
    #[structopt(long, parse(try_from_str), default_value = "all")]
    pub image_link_type: LinkType,
}

fn parse_duration_secs(s: &str) -> Result<Duration> {
    s.parse::<u64>()
        .map(Duration::from_secs)
        .map_err(Into::into)
}

#[derive(Clone, Debug, StructOpt)]
struct ReplaceArgs {
    #[structopt(short = "O", long, parse(from_os_str))]
    output: PathBuf,
    #[structopt(short = "o", long)]
    overwrite: bool,
}

#[derive(StructOpt, Debug, Default, Clone)]
struct Common {
    /// log level. default error
    #[structopt(short = "v", parse(from_occurrences))]
    verbose: u8,

    /// path of input files
    #[structopt(parse(from_os_str), default_value = ".")]
    input: PathBuf,

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
    if let Some(common) = opt.common() {
        init_log(common.verbose)?;
    }
    trace!("opt args: {:?}", opt);
    match opt {
        Opt::Check {
            common,
            checker_args: args,
        } => {
            let paths = common.load_input_paths()?;
            let checker = Checker::new(args);
            do_check(&paths, &checker).await;
        }
        Opt::Replace {
            checker_args,
            common,
            replace_args,
        } => {
            if common.input == replace_args.output && common.input.is_dir() {
                bail!("prohibit output to input folder: {:?}", common.input);
            }
            do_replace(common, checker_args, replace_args).await?;
        }
        Opt::Completion => {
            use structopt::clap::Shell;
            Opt::clap().gen_completions_to(CRATE_NAME.as_str(), Shell::Zsh, &mut stdout());
            return Ok(());
        }
    }
    Ok(())
}

fn check_output_path(output: impl AsRef<Path>, overwrite: bool) -> Result<()> {
    let output = output.as_ref();
    let output_str = output
        .to_str()
        .ok_or_else(|| anyhow!("{:?} to str error", output))?;
    debug!("checking if output {} is available", output_str);
    if !output.exists() {
        info!("creating output dir: {}", output_str);
        create_dir(&output).map_err::<Error, _>(Into::into)?;
    } else if !overwrite && fs::read_dir(&output).map_or(true, |mut dir| dir.next().is_some()) {
        bail!("non empty output dir: {}", output_str);
    }
    Ok(())
}

async fn write_output(output: impl AsRef<Path>, text: &str, overwrite: bool) -> Result<()> {
    let output = output.as_ref();
    debug!(
        "writing replacement result to path {:?}, overwrite: {}",
        output, overwrite
    );
    let parent_dir = output
        .parent()
        .ok_or_else(|| anyhow!("no parent in path: {:?}", output))?;
    if output.is_file() && !overwrite {
        bail!(
            "overwriting args:[--overwrite] of existing file {:?} is not allowed",
            output
        )
    }
    if !parent_dir.exists() {
        info!("creating all parent dir for new output path: {:?}", output);
        create_dir_all(parent_dir).map_err::<Error, _>(Into::into)?;
    }
    write(&output, text).await.map_err::<Error, _>(Into::into)
}

async fn do_replace(common: Common, ch_args: CheckerArgs, re_args: ReplaceArgs) -> Result<()> {
    if matches!(ch_args.image_link_type, LinkType::None) {
        warn!("stopping replace action with args: image_link_type=None");
        return Ok(());
    }

    let paths = common.load_input_paths()?;
    check_output_path(&re_args.output, re_args.overwrite)?;
    // guaranteed to be an absolute path
    let input = common.input.canonicalize()?;
    let checker = Checker::new(ch_args);

    let mut handles = vec![];
    for path in paths {
        let path = path.clone();
        let input = input.clone();
        let checker = checker.clone();
        let sub_path = get_sub_path(input, path.to_path_buf())?;
        // cant use join for absolute path
        debug_assert!(
            sub_path.is_relative(),
            "Forbidden absolute path {:?}",
            sub_path
        );
        let output = re_args.output.join(sub_path);
        let overwrite = re_args.overwrite;
        let handle = tokio::spawn(async move {
            debug!("starting a replace task to {:?} from {:?}", output, path);
            let (text, errs) = checker.embed_base64(&path).await?;
            write_output(output, &text, overwrite).await?;
            debug!(
                "embed_base64 task completed, {} errors found for {:?}",
                errs.len(),
                path
            );
            // report errors
            if !errs.is_empty() {
                let mut msg = "".to_string();
                if !errs.is_empty() {
                    msg += &format!("found {} errors in {:?}\n", errs.len(), path);
                    msg = errs.iter().fold(msg, |mut acc, e| {
                        acc += &format!(
                            "link: {}, cause: {}\n",
                            e.downcast_ref::<Link>().unwrap(),
                            e.root_cause()
                        );
                        acc
                    });
                }
                eprintln!("{}", msg);
            }
            Ok::<_, Error>(())
        });
        handles.push(handle);
    }
    let size = handles.len();
    println!("waiting {} tasks in {:?}", size, input);
    for handle in future::join_all(handles).await {
        if let Err(e) = handle.map_err(Into::into).and_then(|e| e) {
            eprintln!("async task failed: {}", e);
        }
    }
    println!("{} replace tasks completed", size);
    Ok(())
}

/// 返回path相对base多出的路径。如base: /a/b, path: /a/b/c/d/e.md => sub path: c/d/e.md
///
///
/// # Errors
///
/// - 任何相对路径
/// - 相同的path==input且是文件
fn get_sub_path<T: AsRef<Path> + Debug>(base: T, path: T) -> Result<PathBuf> {
    let base = base.as_ref();
    let path = path.as_ref();
    trace!(
        "generating new sub path with input: {:?}, path: {:?}",
        base,
        path
    );
    if base.is_relative() || path.is_relative() {
        bail!(
            "forbiden relative path for input: {:?}, path: {:?}",
            base,
            path
        );
    }

    if base == path {
        trace!("same path {:?} only for input is file", base);
        if base.is_file() {
            bail!("same input path {:?} is file", base);
        }
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("to str "))?
            .parse::<PathBuf>()?;
        return Ok(name);
    }
    let mut stack = vec![];
    for p in path.ancestors() {
        if p == base {
            break;
        }
        let name = p
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("to str error"))?;
        stack.push(name);
    }
    if stack.is_empty() {
        bail!("input {:?} is not an ancestor of path {:?}", base, path);
    }
    let mut new = PathBuf::new();
    while let Some(v) = stack.pop() {
        new.push(v);
    }
    debug!("generated new sub path: {:?} for original: {:?}", new, path);
    Ok(new)
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
    /// 从self.paths中读取md文件paths。
    fn load_input_paths(&self) -> Result<Vec<PathBuf>> {
        let path = self.input.as_path().canonicalize()?;
        let path_str = path
            .to_str()
            .ok_or_else(|| anyhow!("{:?} to str error", path))?;
        info!(
            "loading paths of markdown file from {} with extensions: {:?}",
            path_str, self.extensions
        );
        if !path.exists() {
            bail!("{} does not exist", path_str);
        }

        if path.is_file() {
            if self.recursive {
                bail!("recursive flag [-r] is not available on file: {}", path_str);
            }
            if self.is_markdown_file(&path) {
                return Ok(vec![path.to_path_buf()]);
            } else {
                bail!("{} is not markdown file", path_str,);
            }
        }

        // dir
        if self.recursive {
            let mut paths = vec![];
            self.visite_recursive(path, &mut paths).map(|_| paths)
        } else {
            Ok(path
                .read_dir()?
                .filter(Result::is_ok)
                .map(|dir| dir.unwrap().path())
                .filter(|p| self.is_markdown_file(p))
                .map(|p| p.canonicalize().unwrap())
                .collect())
        }
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
                paths.push(path.canonicalize()?);
            } else {
                log::trace!("ignore un md file: {:?}", path);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_output() -> Result<()> {
        let input = "/root/a/b";
        let path = "test/a.md";
        let res = get_sub_path(input, &format!("{}/{}", input, path))?;
        assert_eq!(res.to_str().unwrap(), path);
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
