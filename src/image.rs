use crate::cmd::CheckerArgs;
use anyhow::{anyhow, Error, Result};
use futures::future;
use futures::TryFutureExt;
use log::*;
use once_cell::sync::Lazy;
use regex::{Captures, Regex};
use reqwest::Client;
use std::collections::HashMap;
use std::convert::AsRef;
use std::convert::TryFrom;
use std::fmt::Display;
use std::fs::File;
use std::io::Read;
use std::string::ToString;
use std::time::SystemTime;
use std::{
    fs::read_to_string,
    path::{Path, PathBuf},
    str::FromStr,
};
use strum::VariantNames;
use strum_macros::AsRefStr;
use strum_macros::{Display, EnumString, EnumVariantNames};
use thiserror::Error;
use tokio::fs as afs;
use url::Url;

static RE_IMAGE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"!\[(.*)\]\((.+)\)").expect("regex init failed for image"));

#[derive(Debug, PartialEq, Eq, Display, EnumString, Clone, Copy)]
#[strum(ascii_case_insensitive)]
pub enum LinkType {
    Local,
    Net,
    All,
    None,
}

#[derive(Debug, Error)]
pub enum LinkCheckError {
    #[error("The link was not found because path does not exist or url is not available")]
    NotFound,
    #[error("path or url is not image file: {0}")]
    NotImageFile(String),
    #[error("unsupported image extension: {0}")]
    UnsupportedExtension(String),
    #[error("invalid image magic numbers: {0:?}")]
    InvalidImageMagicNumber([u8; 4]),

    #[error("request head failed, status: {0}, headers: {1}")]
    HeadFailed(u16, String),
    #[error("")]
    FailedResponse(u16, String),
    #[error("unknown error: {0}")]
    Unknown(String),
    #[error("error: {0}")]
    Other(Error),
}

#[derive(Debug)]
pub struct CheckInfo {
    pub path: String,
    pub image_link: String,
    pub link: Link,
    pub line: String,
    pub row_num: usize,
    pub col_num: usize,
    pub error: Option<LinkCheckError>,
}

#[derive(Debug, PartialEq, Eq, AsRefStr, Clone)]
pub enum Link {
    Local(PathBuf),
    Net(Url),
}

impl Display for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}({})",
            self.as_ref(),
            match self {
                Link::Local(p) => p.to_str().unwrap_or("none"),
                Link::Net(u) => u.as_str(),
            }
        )
    }
}

#[derive(Debug, PartialEq, Eq, Display, EnumString, EnumVariantNames)]
#[strum(ascii_case_insensitive)]
enum ImageFormat {
    Bmp,
    Jpeg,
    Gif,
    Tiff,
    Png,
}

impl TryFrom<&[u8]> for ImageFormat {
    type Error = anyhow::Error;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        static BMP: &[u8; 2] = b"BM"; // BMP
        static GIF: &[u8; 3] = b"GIF"; // GIF
        static PNG: &[u8; 4] = &[137, 80, 78, 71]; // PNG
        static TIFF: &[u8; 3] = &[73, 73, 42]; // TIFF
        static TIFF2: &[u8; 3] = &[77, 77, 42]; // TIFF
        static JPEG: &[u8; 4] = &[255, 216, 255, 224]; // jpeg
        static JPEG2: &[u8; 4] = &[255, 216, 255, 225]; // jpeg canon
        use ImageFormat::*;

        Ok(if value.starts_with(BMP) {
            Bmp
        } else if value.starts_with(GIF) {
            Gif
        } else if value.starts_with(PNG) {
            Png
        } else if value.starts_with(TIFF) || value.starts_with(TIFF2) {
            Tiff
        } else if value.starts_with(JPEG) || value.starts_with(JPEG2) {
            Jpeg
        } else {
            return Err(anyhow!("unknown format image"));
        })
    }
}

#[derive(Clone, Debug)]
pub struct Checker {
    client: Client,
    args: CheckerArgs,
}

impl Checker {
    pub fn new(args: CheckerArgs) -> Self {
        let client = Client::builder()
            .connect_timeout(args.connect_timeout)
            .build()
            .expect("client build error");
        Self { client, args }
    }

    /// 解析path中markdown文件中的link替换为内嵌的base64链接并返回
    /// 替换失败的link与error信息。
    ///
    /// # Errors
    ///
    /// - 如果path不是一个可读文件
    pub async fn embed_base64(&self, path: impl AsRef<Path>) -> Result<(String, Vec<Error>)> {
        let path = path.as_ref();
        let path_str = path
            .to_str()
            .ok_or_else(|| anyhow!("to str error: {:?}", path))?;
        debug!("embedding base64 for {}, args: {:?}", path_str, self.args);
        let text = read_to_string(path)?;
        let link_type = self.args.image_link_type;

        let jobs = RE_IMAGE
            .captures_iter(&text)
            .map(|caps| (caps[0].to_string(), parse_link(path, &caps[2])))
            .filter(|(_, link)| {
                if matches!(
                    (&link, link_type),
                    (Link::Local(_), LinkType::All | LinkType::Local)
                        | (Link::Net(_), LinkType::All | LinkType::Net)
                ) {
                    true
                } else {
                    trace!("skip a link {} for link type: {}", link, link_type);
                    false
                }
            })
            .map(|(all, link)| {
                let client = self.client.clone();
                tokio::spawn(async move {
                    trace!("start loading Link {} coding task", link);
                    check_link(&client, &link)
                        .map_err(Into::into)
                        .and_then(|_| load_image(&client, &link))
                        .and_then(|buf| async move {
                            ImageFormat::try_from(&buf[..])
                                // to base64 encode bytes
                                .map(|fmt| (all, base64::encode(buf), fmt))
                        })
                        // get the context value: if let Ok(link) = e.downcast::<Link>()
                        .map_err(|e: Error| e.context(link.clone()))
                        .await
                })
            })
            .collect::<Vec<_>>();

        let jobs_size = jobs.len();
        trace!("waiting for {} link tasks in path: {}", jobs_size, path_str);

        let mut err_links = vec![];
        let mut links = HashMap::new();
        for job in future::join_all(jobs).await {
            let res: Result<(String, String, ImageFormat)> = match job {
                Ok(v) => v,
                Err(e) => {
                    warn!("{} has async job error: {}", path_str, e);
                    continue;
                }
            };
            match res {
                Ok((all, b64, fmt)) => {
                    links.insert(all, (b64, fmt));
                }
                Err(e) => {
                    debug!(
                        "link {} task failed in {}: {}",
                        e.downcast_ref::<Link>()
                            .map(Link::to_string)
                            .unwrap_or_else(|| "none".to_string()),
                        path_str,
                        e
                    );
                    err_links.push(e);
                }
            };
        }

        trace!(
            "there are {} tasks and {} failed for {}",
            jobs_size,
            err_links.len(),
            path_str
        );
        if links.is_empty() {
            info!("skip replacement because no link is available. all link tasks: {}, failed links: {}", 
                jobs_size, 
                err_links.len()
            );
            return Ok((text, err_links));
        }
        debug!("replacing {} in all {} links for {}", links.len(), jobs_size, path_str);
        Ok((replace(&text, &links), err_links))
    }

    pub async fn check(&self, path: impl AsRef<Path>) -> Result<Vec<CheckInfo>> {
        let path = path.as_ref();
        debug!("checking in path: {:?}, args: {:?}", path, self.args);
        // line mode
        let text = read_to_string(path)?;
        let link_type = self.args.image_link_type;
        let mut tasks = vec![];
        for (row, line) in text.lines().enumerate() {
            for caps in RE_IMAGE.captures_iter(line) {
                // caps[2] is link for RE_IMAGE
                let link = parse_link(path, &caps[2]);
                // filter by link type
                if !matches!(
                    (&link, &link_type),
                    (Link::Local(_), LinkType::All | LinkType::Local)
                        | (Link::Net(_), LinkType::All | LinkType::Net)
                ) {
                    trace!(
                        "skip a link {} for link type: {}",
                        link.to_string(),
                        link_type
                    );
                    continue;
                }

                // async check
                let client = self.client.clone();
                let (line, image_link, path) = (
                    line.to_string(),
                    caps[0].to_string(),
                    path.to_str().unwrap_or("").to_string(),
                );
                let handle = tokio::spawn(async move {
                    let error = check_link(&client, &link)
                        .await
                        .map_or_else(Some, |_| None::<LinkCheckError>);
                    CheckInfo {
                        error,
                        line,
                        link,
                        path,
                        col_num: 0,
                        row_num: row + 1,
                        image_link,
                    }
                });
                tasks.push(handle);
            }
        }
        debug!("waiting {} tasks on path: {:?}", tasks.len(), path);
        let infos = futures::future::join_all(tasks)
            .await
            .into_iter()
            .filter(|info| {
                if let Err(e) = info {
                    error!("async task failed: {} in path: {:?}", e, path);
                    false
                } else {
                    true
                }
            })
            .map(Result::unwrap)
            .collect::<Vec<_>>();

        if log_enabled!(Level::Debug) {
            debug!(
                "there are {} problems in the check path: {:?}",
                infos.iter().filter(|info| info.error.is_some()).count(),
                path
            );
        }
        Ok(infos)
    }
}

/// 联合md文件path与link解析出Link。如果link是一个本地相对path，使用
/// 文件path组合为可用的path。不会验证link的可用性
///
/// `path:tests/resources/test.md, link=./test.png => tests/resources/test.png`
///
/// # panics
///
/// 当本地path不存在parent目录时。如`path=/`
fn parse_link(path: impl AsRef<Path>, link: &str) -> Link {
    let path = path.as_ref();
    trace!("parsing link for path: {:?}, original link: {}", path, link);
    if let Ok(url) = link.parse::<Url>() {
        return Link::Net(url);
    }
    let link_path = Path::new(link);
    // get new link path
    let link_path = if link_path.is_relative() {
        // tests/resources/test.md : link=./test.png => tests/resources/test.png
        path.parent()
            .map(|parent| parent.join(link_path))
            .unwrap_or_else(|| {
                panic!(
                    "join failed: path: {:?}, parent {:?}, link: {}",
                    path,
                    path.parent(),
                    link
                )
            })
    } else {
        link_path.to_path_buf()
    };
    Link::Local(link_path)
}

/// 检查link是否是一个可用的image链接。
///
/// - 对于url link，使用http head方法访问检查content type
/// - 对于local path使用extension与magic number双重检查
async fn check_link(client: &Client, link: &Link) -> Result<(), LinkCheckError> {
    trace!("checking link: {}", link.to_string());
    match link {
        Link::Local(p) => check_link_local(p),
        Link::Net(url) => check_link_net(client, url.as_str()).await,
    }
}

fn check_link_local(p: &Path) -> Result<(), LinkCheckError> {
    // 1. check if image is file
    if !p.is_file() {
        return Err(LinkCheckError::NotImageFile(
            "path does not exist or is file".to_string(),
        ));
    }

    // 2. check extension
    let extension = p
        .extension()
        .and_then(|s| s.to_str())
        .ok_or_else(|| LinkCheckError::Unknown(format!("to str error: {:?}", p)))?;
    ImageFormat::from_str(extension).map_err(|e| {
        debug!("unsupported image extension: {}, error: {}", extension, e);
        LinkCheckError::UnsupportedExtension(extension.to_lowercase())
    })?;

    // 3. check magic
    let mut buf = [0; 4];
    File::open(p)
        .map_err(|e| {
            debug!("open path {:?} failed: {}", p, e);
            LinkCheckError::Other(e.into())
        })?
        .read(&mut buf)
        .map_err(|e| {
            debug!("read path {:?} failed: {}", p, e);
            LinkCheckError::Other(e.into())
        })?;

    ImageFormat::try_from(&buf[..])
        .map_err(|e| {
            debug!("parse image format error: {}, magic num: {:?}", e, buf);
            LinkCheckError::InvalidImageMagicNumber(buf)
        })
        .map(|_| ())
}

async fn check_link_net(client: &Client, url: &str) -> Result<(), LinkCheckError> {
    // send head request
    let resp = client
        .head(url)
        .send()
        .await
        .map_err(|e| LinkCheckError::Other(e.into()))?;

    let status = resp.status().as_u16();
    if resp.status().is_success() {
        let head_val = resp
            .headers()
            .get("Content-Type")
            .ok_or_else(|| {
                debug!("not found content-type in headers: {:?}", resp.headers());
                LinkCheckError::HeadFailed(status, "not found content-type in headers".to_string())
            })?
            .to_str()
            .map_err(Into::into)
            .map_err(LinkCheckError::Other)?
            .to_ascii_lowercase();

        // Use content type to check image types
        if !ImageFormat::VARIANTS
            .iter()
            .any(|name| head_val.contains(&name.to_ascii_lowercase()))
        {
            debug!("unsupported image in header content type: {}", head_val);
            return Err(LinkCheckError::UnsupportedExtension(format!(
                "content-type: {}",
                head_val
            )));
        }
    } else {
        // failed on status
        debug!(
            "unsuccessful status: {}, headers: {:?}",
            status,
            resp.headers()
        );
        return Err(LinkCheckError::HeadFailed(
            status,
            "unsuccessful status".to_string(),
        ));
    }
    Ok(())
}

async fn load_image(client: &Client, link: &Link) -> Result<Vec<u8>> {
    match link {
        Link::Local(path) => {
            trace!("reading from path: {:?}", path.to_str());
            afs::read(path).await.map_err(Into::into)
        }
        Link::Net(url) => {
            trace!("sending get request to load image: {}", url);
            let resp = client.get(url.as_str()).send().await?;
            if !resp.status().is_success() {
                warn!(
                    "request failed for url: {}, status: {}, body.text: {}",
                    url,
                    resp.status(),
                    resp.text().await?
                );
                return Err(anyhow!("failed request url: {}", url));
            }
            resp.bytes().await.map(|b| b.to_vec()).map_err(Into::into)
        }
    }
}

fn get_link_name(link: &str) -> String {
    link.parse::<Url>()
        .ok()
        .and_then(|url| {
            url.path_segments()
                .and_then(|items| items.last().map(|s| s.to_string()))
        })
        .or_else(|| {
            Path::new(link)
                .file_name()
                .and_then(|s| s.to_str().map(|s| s.to_string()))
        })
        .unwrap_or_else(|| {
            let name = "none".to_string();
            warn!(
                "not found filename in link: {}, replace it with the default name: {}",
                link, name
            );
            name
        })
}

/// 从text中替换所有存在links中的链接为value.base64的硬编码格式值：
///
/// - 替换link为：`![alt_text][id]`
/// - `[id]:data:image/png;base64,__b64code`添加到文件最后
///
/// id使用`count_filename`的格式。count为links的数量，filename从link中取，
/// 如果无法找到filename使用默认值`count_none`
fn replace(text: &str, links: &HashMap<String, (String, ImageFormat)>) -> String {
    // count for replacement
    let mut count = 0;
    let mut encoded_text = format!(
        "\n\n<!-- auto generated by mdutil at {} -->\n\n",
        humantime::format_rfc3339(SystemTime::now())
    );
    let mut new_text = RE_IMAGE
        .replace_all(text, |caps: &Captures| {
            let (original, link) = (caps[0].to_string(), &caps[2]);
            // 1. get key
            if let Some((b64, fmt)) = links.get(&original) {
                let id = format!("{}_{}", count, get_link_name(link));
                // 2. get replaced name, use id to index `![alt text][id]`
                let new = format!("![{}][{}]", &caps[1], id);
                info!("replacing image link `{}` with `{}`", original, new);

                // 3. append to the end
                encoded_text.push_str(&format!(
                    "[{}]:data:image/{};base64,{}\n",
                    id,
                    fmt.to_string().to_lowercase(),
                    b64
                ));

                count += 1;
                new
            } else {
                // Not selected for replacement
                original
            }
        })
        .to_string();

    // merge to the end
    new_text += &encoded_text;
    new_text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::DATA_DIR;
    use std::time::Duration;

    static DATA: &str = r#"
# markdown util

the test

![absolutely local image](/home/navyd/Workspaces/projects/blog-resources/assets/images/0c1c6c89-5dfb-4a5e-a4c5-6aac1b299b37.png)

![test image](test/test.png)

![not exist image](test/_not_exists_test.png)

![](https://www.baidu.com/img/PCtm_d9c8750bed0b3c7d089fa7d55720d6cf.png)

![](https://fsafthe234notexist.com/a/b/c_aaaa/a.png)"#;

    static CLIENT: Lazy<Client> = Lazy::new(|| Client::builder().build().unwrap());
    static ARGS: CheckerArgs = CheckerArgs {
        connect_timeout: Duration::from_secs(3),
        image_link_type: LinkType::All,
    };

    static CHECKER: Lazy<Checker> = Lazy::new(|| Checker::new(ARGS.clone()));

    #[tokio::test]
    async fn test_check() -> Result<()> {
        let checker = Checker::new(ARGS.clone());
        let path = DATA_DIR.join("test.md");

        let infos = checker.check(path).await?;
        assert_eq!(infos.len(), 5);
        assert_eq!(infos.iter().filter(|info| info.error.is_some()).count(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn convert_image_to_base64() -> Result<()> {
        let checker = CHECKER.clone();
        let path = DATA_DIR.join("test.md");
        let (new, faileds) = checker.embed_base64(&path).await?;
        let old = read_to_string(&path)?;

        assert_eq!(faileds.len(), 2);
        assert!(faileds.iter().all(|e| {
            let s = e.root_cause().to_string();
            e.downcast_ref::<Link>().is_some()
                && (s.contains("not exist or is file") || s.contains("unexpected EOF"))
        }));
        assert!(!new.is_empty());
        assert!(new.len() > old.len());
        // to compare first line
        assert_eq!(new.lines().next(), old.lines().next());
        assert!(new.lines().count() > old.lines().count());
        let link = "![test image](test.png)";
        assert!(!new.contains(link) && old.contains(link));

        let link = "![not exist image](test/_not_exists_test.png)";
        assert!(new.contains(link) && old.contains(link));

        let link = "![](https://www.baidu.com/img/PCtm_d9c8750bed0b3c7d089fa7d55720d6cf.png)";
        assert!(!new.contains(link) && old.contains(link));

        let link = "![](https://fsafthe234notexist.com/a/b/c_aaaa/a.png)";
        assert!(new.contains(link) && old.contains(link));

        Ok(())
    }

    #[tokio::test]
    async fn test_check_links() -> Result<()> {
        assert!(check_link(
            &CLIENT,
            &Link::Net(
                "https://www.baidu.com/img/PCtm_d9c8750bed0b3c7d089fa7d55720d6cf.png".parse()?
            ),
        )
        .await
        .is_ok());
        assert!(check_link(
            &CLIENT,
            &Link::Net("https://fsafthe234notexist.com/a/b/c_aaaa/a.png".parse()?)
        )
        .await
        .is_err());

        assert!(check_link(
            &CLIENT,
            &Link::Local(format!("{}/_not_exists_test.png", DATA_DIR.to_str().unwrap()).parse()?)
        )
        .await
        .is_err());
        assert!(check_link(
            &CLIENT,
            &Link::Local(format!("{}/test.png", DATA_DIR.to_str().unwrap()).parse()?)
        )
        .await
        .is_ok());
        Ok(())
    }

    #[test]
    fn valid_image_format() -> Result<()> {
        let buf = std::fs::read(format!("{}/test.png", DATA_DIR.to_str().unwrap()))?;
        assert_eq!(ImageFormat::Png, ImageFormat::try_from(&buf[..])?);

        let buf = std::fs::read(format!("{}/test.md", DATA_DIR.to_str().unwrap()))?;
        assert!(ImageFormat::try_from(&buf[..]).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn test_load_image() -> Result<()> {
        let link = "https://www.baidu.com/img/PCtm_d9c8750bed0b3c7d089fa7d55720d6cf.png";
        assert!(!load_image(&CLIENT, &Link::Net(link.parse()?))
            .await?
            .is_empty());

        let link = format!("{}/test.png", DATA_DIR.to_str().unwrap());
        assert!(!load_image(&CLIENT, &Link::Local(link.parse()?))
            .await?
            .is_empty());

        let link = format!("{}/_not/exist/_file.png", DATA_DIR.to_str().unwrap());
        assert!(load_image(&CLIENT, &Link::Local(link.parse()?))
            .await
            .is_err());
        Ok(())
    }

    #[test]
    fn test_parse_link() -> Result<()> {
        let mut md_path = DATA_DIR.to_path_buf();
        md_path.push("test.md");

        let link = "https://www.baidu.com/img/PCtm_d9c8750bed0b3c7d089fa7d55720d6cf.png";
        assert_eq!(parse_link(&md_path, link), Link::Net(link.parse()?));

        let link = "test.png";
        assert_eq!(
            parse_link(&md_path, link),
            Link::Local(
                format!(
                    "{}/{}",
                    md_path.parent().and_then(|s| s.to_str()).unwrap(),
                    link
                )
                .parse()?
            )
        );

        let link = "_not/exist/_file.png";
        assert_eq!(
            parse_link(&md_path, link),
            Link::Local(
                format!(
                    "{}/{}",
                    md_path.parent().and_then(|s| s.to_str()).unwrap(),
                    link
                )
                .parse()?
            )
        );

        // 不验证path
        let link = "_not/exis.fsdfsdf.sdfdsf./sdfds.fdsfs";
        md_path.push("noexist_dir");
        assert_eq!(
            parse_link(&md_path, link),
            Link::Local(
                format!(
                    "{}/{}",
                    md_path.parent().and_then(|s| s.to_str()).unwrap(),
                    link
                )
                .parse()?
            )
        );
        Ok(())
    }

    #[test]
    fn replace_with_links() -> Result<()> {
        let mut links = HashMap::new();
        links.insert(
            "![test image](test/test.png)".to_string(),
            ("base64testaaabb".to_string(), ImageFormat::Png),
        );
        links.insert(
            "![](https://www.baidu.com/img/PCtm_d9c8750bed0b3c7d089fa7d55720d6cf.png)".to_string(),
            ("base64testaaabbcccccc".to_string(), ImageFormat::Png),
        );
        let old = DATA;
        let new = replace(old, &links);
        assert!(new.len() > old.len());
        assert!(!new.contains("![test image](test/test.png)"));
        // image id with base64 encoded: `[id]:data:image/png;base64,base64_encode`
        assert!(new.contains("[1_PCtm_d9c8750bed0b3c7d089fa7d55720d6cf.png]:data:image/png;base64,base64testaaabbcccccc"));
        Ok(())
    }
}
