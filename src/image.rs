use anyhow::{anyhow, Error, Result};
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
use std::time::{Duration, SystemTime};
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

#[derive(Debug, PartialEq, Eq, AsRefStr)]
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

impl FromStr for Link {
    type Err = Error;

    /// 解析字符串到Link。如果是一个Url类型或是一个path类型
    fn from_str(link: &str) -> Result<Self, Self::Err> {
        match link.parse::<Url>() {
            Ok(url) => return Ok(Link::Net(url)),
            Err(e) => log::trace!("invalid url for link: {}, error: {}", link, e),
        };

        Ok(Link::Local(Path::new(&link).to_path_buf()))
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

    /// 解析path中markdown文件中的link替换为内嵌的base64链接
    pub async fn embed_base64(&self, path: impl AsRef<Path>) -> Result<String> {
        let path = path.as_ref();
        let path_str = path.to_str().ok_or_else(|| anyhow!("unlink"))?;
        let text = read_to_string(path)?;
        // 1. get all links with key caps[0]
        let jobs = RE_IMAGE
            .captures_iter(&text)
            .map(|caps| (caps[0].to_string(), parse_link(path, &caps[2])))
            .filter(|(all, link)| {
                // filter unlink
                if let Err(e) = link {
                    debug!("skip the link `{}` that failed to parse: {}", all, e);
                    false
                } else {
                    // 2. filter by link_type and check link
                    matches!(
                        (link.as_ref().unwrap(), self.args.link_type),
                        (Link::Local(_), LinkType::All | LinkType::Local)
                            | (Link::Net(_), LinkType::All | LinkType::Net)
                    )
                }
            })
            .map(|(all, link)| (all, link.unwrap()))
            .map(|(all, link)| {
                trace!("start loading Link {} coding task", link);
                let client = self.client.clone();
                tokio::spawn(async move {
                    // log::trace!("checking if link {:?} is available", link);
                    if let Err(e) = check_link(&client, &link).await {
                        return Err(anyhow!(
                            "found link {} is unavailable: {}",
                            link.to_string(),
                            e
                        ));
                    }
                    // 3. load image in parallel
                    let buf = load_image(&client, &link).await?;
                    // 4. valid magic numbers
                    let fmt = match ImageFormat::try_from(&buf[..]) {
                        Ok(v) => v,
                        Err(e) => {
                            debug!("the link: `{:?}` is not an image file: {}", link, e);
                            return Err(e);
                        }
                    };
                    // to base64 encode bytes
                    Ok::<(String, String, ImageFormat), Error>((all, base64::encode(buf), fmt))
                })
            })
            .collect::<Vec<_>>();

        trace!(
            "waiting for {} link tasks in path: {}",
            jobs.len(),
            path_str
        );
        let links = futures::future::join_all(jobs)
            .await
            .into_iter()
            .map(|res| res.map_err(Into::into).and_then(|v| v))
            .filter(|res| {
                if let Err(e) = res {
                    log::info!("skipping {} by failed link task: {}", path_str, e);
                    false
                } else {
                    true
                }
            })
            .map(Result::unwrap)
            .map(|(key, b64, fmt)| (key, (b64, fmt)))
            .collect::<HashMap<String, (String, ImageFormat)>>();

        // 5. replace links with key val and append base64 to the end
        Ok(replace(&text, &links))
    }

    pub async fn check(&self, path: impl AsRef<Path>) -> Result<Vec<CheckInfo>> {
        let path = path.as_ref();
        // line mode
        let text = read_to_string(path)?;
        let link_type = self.args.link_type;
        let mut tasks = vec![];
        for (row, line) in text.lines().enumerate() {
            for caps in RE_IMAGE.captures_iter(line) {
                // caps[2] is link for RE_IMAGE
                let link = parse_link(path, &caps[2])?;
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
                    log::error!("async task failed: {} in path: {:?}", e, path);
                    false
                } else {
                    true
                }
            })
            .map(Result::unwrap)
            .collect::<Vec<_>>();

        if log_enabled!(Level::Info) {
            info!(
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
fn parse_link(path: impl AsRef<Path>, link: &str) -> Result<Link> {
    let path = path.as_ref();
    match link.parse::<Url>() {
        Ok(url) => return Ok(Link::Net(url)),
        Err(e) => trace!("invalid url for link: {}, error: {}", link, e),
    };
    let link_path = Path::new(link);
    // get new link path
    let link_path = if link_path.is_relative() {
        // tests/resources/test.md : link=./test.png => tests/resources/test.png
        path.parent()
            .map(|parent| parent.join(link_path))
            .ok_or_else(|| {
                anyhow!(
                    "join failed: parent {:?}, sub {:?}",
                    path.parent(),
                    link_path
                )
            })?
    } else {
        link_path.to_path_buf()
    };
    Ok(Link::Local(link_path))
}

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
    if log::log_enabled!(log::Level::Trace) {
        log::trace!(
            "got resp for url: {}, status: {}, headers: {:?}",
            url,
            resp.status(),
            resp.headers().iter().collect::<Vec<_>>()
        );
    }

    if resp.status().is_success() {
        let status = resp.status().as_u16();
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
    }
    Ok(())
}

async fn load_image(client: &Client, link: &Link) -> Result<Vec<u8>> {
    match link {
        Link::Local(path) => {
            log::trace!("reading from path: {:?}", path.to_str());
            afs::read(path).await.map_err(Into::into)
        }
        Link::Net(url) => {
            log::trace!("requesting with url: {}", url);
            let resp = client.get(url.as_str()).send().await?;
            if !resp.status().is_success() {
                log::warn!(
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
    log::debug!("start using regular substitution links: {}", links.len());
    let mut new_text = RE_IMAGE
        .replace_all(text, |caps: &Captures| {
            let (original, link) = (caps[0].to_string(), &caps[2]);
            // 1. get key
            if let Some((b64, fmt)) = links.get(&original) {
                let id = match link.parse::<Link>() {
                    Ok(Link::Local(p)) => p
                        .file_name()
                        .and_then(|s| s.to_str().map(|s| s.to_string())),
                    Ok(Link::Net(url)) => url
                        .path_segments()
                        .and_then(|items| items.last().map(|s| s.to_string())),
                    // ignore
                    _ => return original,
                }
                // `0_file.jpg`
                .map(|s| format!("{}_{}", count, s))
                // `0_none`
                .unwrap_or_else(|| {
                    log::debug!(
                        "not found filename in link: {}, replace it with the default name: none",
                        link
                    );
                    format!("{}_none", count)
                });

                // 2. get replaced name, use id to index `![alt text][id]`
                let new = format!("![{}][{}]", &caps[1], id);
                log::info!("replacing link `{}` with `{}`", original, new);

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

// #[cfg(test)]
// mod tests {
//     use super::*;

//     static DATA: &str = r#"
// # markdown util

// the test

// ![absolutely local image](/home/navyd/Workspaces/projects/blog-resources/assets/images/0c1c6c89-5dfb-4a5e-a4c5-6aac1b299b37.png)

// ![test image](test/test.png)

// ![not exist image](test/_not_exists_test.png)

// ![](https://www.baidu.com/img/PCtm_d9c8750bed0b3c7d089fa7d55720d6cf.png)

// ![](https://fsafthe234notexist.com/a/b/c_aaaa/a.png)"#;

//     static CLIENT: Lazy<Client> = Lazy::new(|| Client::builder().build().unwrap());

//     static DATA_DIR: Lazy<PathBuf> = Lazy::new(|| "tests/resources".parse().unwrap());

//     #[tokio::test]
//     async fn test_check() -> Result<()> {
//         let checker = Checker::new();
//         let mut path = DATA_DIR.clone();
//         path.push("test.md");

//         let infos = checker.check(path, LinkType::All).await?;
//         assert_eq!(infos.len(), 5);
//         assert_eq!(infos.iter().filter(|info| info.error.is_some()).count(), 2);
//         Ok(())
//     }

//     #[tokio::test]
//     async fn convert_image_to_base64() -> Result<()> {
//         let converter = Checker::new();
//         let old = DATA;
//         let new = converter.embed_base64(old, LinkType::All).await?;
//         assert!(!new.is_empty());
//         assert!(new.len() > old.len());
//         assert!(new.lines().count() > old.lines().count());
//         assert!(new.contains("# markdown util"));
//         assert!(!new.contains("![test image](test/test.png)"));

//         assert!(new.contains("![not exist image](test/_not_exists_test.png)"));
//         assert!(new.contains("![](https://fsafthe234notexist.com/a/b/c_aaaa/a.png)"));

//         let new = converter.embed_base64(old, LinkType::Local).await?;
//         assert!(new.contains("![](https://fsafthe234notexist.com/a/b/c_aaaa/a.png)"));
//         assert!(new
//             .contains("![](https://www.baidu.com/img/PCtm_d9c8750bed0b3c7d089fa7d55720d6cf.png)"));
//         assert!(new.contains("![not exist image](test/_not_exists_test.png)"));
//         assert!(!new.contains("![test image](test/test.png)"));

//         let new = converter.embed_base64(old, LinkType::Net).await?;
//         assert!(!new
//             .contains("![](https://www.baidu.com/img/PCtm_d9c8750bed0b3c7d089fa7d55720d6cf.png)"));
//         Ok(())
//     }

//     #[tokio::test]
//     async fn test_check_links() -> Result<()> {
//         assert!(check_link(
//             &CLIENT,
//             &Link::Net(
//                 "https://www.baidu.com/img/PCtm_d9c8750bed0b3c7d089fa7d55720d6cf.png".parse()?
//             ),
//         )
//         .await
//         .is_ok());
//         assert!(check_link(
//             &CLIENT,
//             &Link::Net("https://fsafthe234notexist.com/a/b/c_aaaa/a.png".parse()?)
//         )
//         .await
//         .is_err());

//         assert!(check_link(
//             &CLIENT,
//             &Link::Local(format!("{}/_not_exists_test.png", DATA_DIR.to_str().unwrap()).parse()?)
//         )
//         .await
//         .is_err());
//         assert!(check_link(
//             &CLIENT,
//             &Link::Local(format!("{}/test.png", DATA_DIR.to_str().unwrap()).parse()?)
//         )
//         .await
//         .is_ok());
//         Ok(())
//     }

//     #[test]
//     fn valid_image_format() -> Result<()> {
//         let buf = std::fs::read(format!("{}/test.png", DATA_DIR.to_str().unwrap()))?;
//         assert_eq!(ImageFormat::Png, ImageFormat::try_from(&buf[..])?);

//         let buf = std::fs::read(format!("{}/test.md", DATA_DIR.to_str().unwrap()))?;
//         assert!(ImageFormat::try_from(&buf[..]).is_err());
//         Ok(())
//     }

//     #[tokio::test]
//     async fn test_load_image() -> Result<()> {
//         let link = "https://www.baidu.com/img/PCtm_d9c8750bed0b3c7d089fa7d55720d6cf.png";
//         assert!(!load_image(&CLIENT, &Link::Net(link.parse()?))
//             .await?
//             .is_empty());

//         let link = format!("{}/test.png", DATA_DIR.to_str().unwrap());
//         assert!(!load_image(&CLIENT, &Link::Local(link.parse()?))
//             .await?
//             .is_empty());

//         let link = format!("{}/_not/exist/_file.png", DATA_DIR.to_str().unwrap());
//         assert!(load_image(&CLIENT, &Link::Local(link.parse()?))
//             .await
//             .is_err());
//         Ok(())
//     }

//     #[test]
//     fn test_parse_link() -> Result<()> {
//         let mut md_path = DATA_DIR.to_path_buf();
//         md_path.push("test.md");

//         let link = "https://www.baidu.com/img/PCtm_d9c8750bed0b3c7d089fa7d55720d6cf.png";
//         assert_eq!(parse_link(&md_path, link)?, Link::Net(link.parse()?));

//         let link = "test.png";
//         assert_eq!(
//             parse_link(&md_path, link)?,
//             Link::Local(
//                 format!(
//                     "{}/{}",
//                     md_path.parent().and_then(|s| s.to_str()).unwrap(),
//                     link
//                 )
//                 .parse()?
//             )
//         );

//         let link = "_not/exist/_file.png";
//         assert_eq!(
//             parse_link(&md_path, link)?,
//             Link::Local(
//                 format!(
//                     "{}/{}",
//                     md_path.parent().and_then(|s| s.to_str()).unwrap(),
//                     link
//                 )
//                 .parse()?
//             )
//         );

//         // 不验证path
//         let link = "_not/exis.fsdfsdf.sdfdsf./sdfds.fdsfs";
//         md_path.push("noexist_dir");
//         assert_eq!(
//             parse_link(&md_path, link)?,
//             Link::Local(
//                 format!(
//                     "{}/{}",
//                     md_path.parent().and_then(|s| s.to_str()).unwrap(),
//                     link
//                 )
//                 .parse()?
//             )
//         );
//         Ok(())
//     }

//     #[test]
//     fn replace_with_links() -> Result<()> {
//         let mut links = HashMap::new();
//         links.insert(
//             "![test image](test/test.png)".to_string(),
//             ("base64testaaabb".to_string(), ImageFormat::Png),
//         );
//         links.insert(
//             "![](https://www.baidu.com/img/PCtm_d9c8750bed0b3c7d089fa7d55720d6cf.png)".to_string(),
//             ("base64testaaabbcccccc".to_string(), ImageFormat::Png),
//         );
//         let old = DATA;
//         let new = replace(old, &links);
//         assert!(new.len() > old.len());
//         assert!(!new.contains("![test image](test/test.png)"));
//         // image id with base64 encoded: `[id]:data:image/png;base64,base64_encode`
//         assert!(new.contains("[1_PCtm_d9c8750bed0b3c7d089fa7d55720d6cf.png]:data:image/png;base64,base64testaaabbcccccc"));
//         Ok(())
//     }
// }
