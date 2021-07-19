use anyhow::{anyhow, Error, Result};
use futures::{stream, StreamExt};
use once_cell::sync::Lazy;
use regex::{Captures, Regex};
use reqwest::Client;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::string::ToString;
use std::time::{Instant, SystemTime};
use std::{
    fs::read_to_string,
    path::{Path, PathBuf},
    str::FromStr,
};
use strum_macros::{Display, EnumString};
use tokio::fs as afs;
use url::Url;

static RE_IMAGE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"!\[(.*)\]\((.+)\)").expect("regex init failed for image"));

#[derive(Debug, PartialEq, Eq, Display)]
pub enum LinkType {
    Local,
    Net,
    All,
}

pub enum LinkError {
    NotFound(Link),
    NotImage(Link),
    Unreachable(Link),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Link {
    Local(PathBuf),
    Net(Url),
    Unknown(String),
}

impl FromStr for Link {
    type Err = Error;

    /// 解析字符串到Link。如果是一个Url类型或是一个path类型
    fn from_str(link: &str) -> Result<Self, Self::Err> {
        match link.parse::<Url>() {
            Ok(url) => return Ok(Link::Net(url)),
            Err(e) => log::trace!("invalid url for link: {}, error: {}", link, e),
        };

        let p = Path::new(&link);
        // check if exists
        if let Err(e) = p.canonicalize() {
            log::trace!("invalid local path for link: {}, error: {}", link, e);
        } else {
            return Ok(Link::Local(p.to_path_buf()));
        }

        log::debug!("unknown link: {}", link);
        Ok(Link::Unknown(link.to_string()))
    }
}

#[derive(Debug, PartialEq, Eq, Display, EnumString)]
enum ImageFormat {
    #[strum(ascii_case_insensitive)]
    Bmp,
    #[strum(ascii_case_insensitive)]
    Jpeg,
    #[strum(ascii_case_insensitive)]
    Gif,
    #[strum(ascii_case_insensitive)]
    Tiff,
    #[strum(ascii_case_insensitive)]
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

pub async fn check(context: &str, link_type: LinkType) -> Result<()> {
    todo!()
}

pub async fn convert(context: &str, link_type: LinkType) -> Result<String> {
    // 1. get all links with key caps[0]

    // 2. filter by link_type and check link

    // 3. load image and base64 encode in parallel

    // 4. 
    todo!()
}

async fn check_link(link: Link) -> Result<()> {
    match link {
        Link::Local(p) => {
            let path_str = p.to_str().ok_or_else(|| anyhow!("to str error"))?;
            if !p.is_file() {
                return Err(anyhow!("the link `{}` is not a file", path_str));
            }

            if ImageFormat::from_str(
                p.file_name()
                    .and_then(|s| s.to_str())
                    .ok_or_else(|| anyhow!("to str error"))?,
            )
            .is_err()
            {
                return Err(anyhow!("the link {} is not a image", path_str));
            }
        }
        Link::Net(url) => {
            log::trace!("checking if the url `{}` is available", url);
            let resp = Client::builder().build()?.head(url.as_str()).send().await?;
            if log::log_enabled!(log::Level::Trace) {
                log::trace!(
                    "got resp for url: {}, status: {}, headers: {:?}",
                    url,
                    resp.status(),
                    resp.headers().iter().collect::<Vec<_>>()
                );
            }

            if !resp.status().is_success()
                || resp
                    .headers()
                    .get("Content-Type")
                    .and_then(|val| val.to_str().map(|s| !s.contains("image")).ok())
                    .unwrap_or(false)
            {
                return Err(anyhow!(
                    "unavailable url: {}, resp status: {}, headers: {:?}",
                    url,
                    resp.status(),
                    resp.headers().iter().collect::<Vec<_>>()
                ));
            }
        }
        Link::Unknown(s) => {
            return Err(anyhow!("unknown link: {}", s));
        }
    }
    Ok(())
}

fn parse_links(context: &str) -> HashMap<String, Link> {
    todo!()
}

pub async fn convert_base64(context: &str, link_type: LinkType) -> Result<String> {
    // 1. parse link in for regex captures
    let (context, links) = replace_links(context, link_type);

    // async tasks
    log::debug!("starting to execute {} link tasks", links.len());
    let jobs = links
        .into_iter()
        .map(|(id, link)| {
            tokio::spawn(async move {
                // 2. load image as bytes
                let buf = load_image(&link).await?;
                // 3. valid the link is image
                let fmt = match ImageFormat::try_from(&buf[..]) {
                    Ok(v) => v,
                    Err(e) => {
                        log::warn!("the link: `{:?}` is not an image file", link);
                        return Err(e);
                    }
                };
                // 4. to base64 encode bytes
                Ok::<(String, ImageFormat, String), Error>((id, fmt, base64::encode(buf)))
            })
        })
        .collect::<Vec<_>>();

    log::trace!("waiting for {} link tasks", jobs.len());
    let res = futures::future::join_all(jobs)
        .await
        .into_iter()
        .map(|e| e.map_err(Into::into).and_then(|v| v))
        .collect::<Vec<Result<(String, ImageFormat, String), Error>>>();
    if log::log_enabled!(log::Level::Debug) {
        log::debug!(
            "{} tasks completed and {} failed",
            res.len(),
            res.iter().filter(|r| r.is_err()).count()
        );
    }

    // check if has any error
    if let Some(Err(e)) = res.iter().find(|r| r.is_err()) {
        return Err(anyhow!("convert base64 failed: {}", e));
    }

    log::trace!("appending {} links to the end", res.len());
    let mut context = context;
    context.push_str(&format!(
        "\n\n<!-- auto generated by mdutil at {} -->\n\n",
        humantime::format_rfc3339(SystemTime::now())
    ));
    // 5. append link with base64
    Ok(res.into_iter().fold(context, |mut acc, r| {
        if let Ok((id, fmt, b64)) = r {
            acc.push_str(&format!(
                "[{}]:data:image/{};base64,{}\n",
                id,
                fmt.to_string().to_lowercase(),
                b64
            ));
        }
        acc
    }))
}

fn replace_links(context: &str, link_type: LinkType) -> (String, Vec<(String, Link)>) {
    let mut id_links = vec![];
    let mut count = 0;
    let new_context = RE_IMAGE.replace_all(context, |caps: &Captures| {
        let (original, link_str) = (&caps[0], &caps[2]);
        match link_str.parse::<Link>() {
            Ok(link) => {
                // get replaced id with count and filename
                let id = match (&link, &link_type) {
                    (Link::Local(p), LinkType::All | LinkType::Local) => {
                        p.file_name().and_then(|s| s.to_str())
                    }
                    (Link::Net(url), LinkType::All | LinkType::Net) => {
                        url.path_segments().and_then(|c| c.last())
                    }
                    (_, _) => {
                        log::debug!(
                            "skip replacing link `{}` due to LinkType: {}",
                            link_str,
                            link_type
                        );
                        return original.to_string();
                    }
                }
                // `1_file.jpg`
                .map(|s| format!("{}_{}", count, s))
                // `1_none`
                .unwrap_or_else(|| {
                    log::debug!(
                        "not found filename in link: {}, replace it with the default name: none",
                        link_str
                    );
                    format!("{}_none", count)
                });

                // use id to index `![image][name]`
                let new = format!("![{}][{}]", &caps[1], id);
                log::debug!("replacing link `{}` with `{}`", original, new);
                id_links.push((id, link));
                count += 1;
                new
            }
            Err(e) => {
                log::warn!("parse link `{}` error: {}", link_str, e);
                original.to_string()
            }
        }
    });
    (new_context.into(), id_links)
}

async fn load_image(link: &Link) -> Result<Vec<u8>> {
    match link {
        Link::Local(path) => {
            log::trace!("reading from path: {:?}", path.to_str());
            afs::read(path).await.map_err(Into::into)
        }
        Link::Net(url) => {
            log::trace!("requesting with url: {}", url);
            let resp = reqwest::get(url.as_str()).await?;
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
        Link::Unknown(s) => Err(anyhow!("unknown link: {}", s)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static DATA: &str = r#"
# markdown util

the test

![absolutely local image](/home/navyd/Workspaces/projects/blog-resources/assets/images/0c1c6c89-5dfb-4a5e-a4c5-6aac1b299b37.png)

![test image](test/test.png)

![not exist image](test/_not_exists_test.png)

![](https://www.baidu.com/img/PCtm_d9c8750bed0b3c7d089fa7d55720d6cf.png)

![](https://fsafthe234notexist.com/a/b/c_aaaa/a.png)"#;

    #[tokio::test]
    async fn convert_image_to_base64() -> Result<()> {
        let old = DATA;
        let new = convert_base64(old, LinkType::All).await?;
        assert!(!new.is_empty());
        assert!(new.contains("# markdown util"));
        assert!(!new.contains("![test image](test/test.png)"));

        assert!(new.len() > old.len());
        assert!(new.lines().count() > old.lines().count());

        let old = format!("{}\n\n![not exist image](test/_not_exists_test.png)", DATA);
        let res = convert_base64(&old, LinkType::All).await;
        assert!(res.is_err());

        Ok(())
    }

    #[test]
    fn replace() -> Result<()> {
        let mut count = 0;
        let result = RE_IMAGE.replace_all(DATA, |caps: &Captures| {
            count += 1;
            format!("![{}][{}]", &caps[1], count)
        });

        log::info!("{}", result);
        Ok(())
    }

    #[test]
    fn valid_image_format() -> Result<()> {
        let buf = std::fs::read("test/test.png")?;
        assert_eq!(ImageFormat::Png, ImageFormat::try_from(&buf[..])?);
        Ok(())
    }

    #[tokio::test]
    async fn test_load_image() -> Result<()> {
        let link = "https://www.baidu.com/img/PCtm_d9c8750bed0b3c7d089fa7d55720d6cf.png";
        assert!(!load_image(&Link::Net(link.parse()?)).await?.is_empty());

        let link = "test/test.png";
        assert!(!load_image(&Link::Local(link.parse()?)).await?.is_empty());

        let link = "_not/exist/_file.png";
        assert!(load_image(&Link::Local(link.parse()?)).await.is_err());
        Ok(())
    }

    #[test]
    fn parse_link() -> Result<()> {
        let link = "https://www.baidu.com/img/PCtm_d9c8750bed0b3c7d089fa7d55720d6cf.png";
        assert_eq!(link.parse::<Link>()?, Link::Net(link.parse()?));

        let link = "test/test.png";
        assert_eq!(link.parse::<Link>()?, Link::Local(PathBuf::from(link)));

        let link = "_not/exist/_file.png";
        assert!(link.parse::<Link>().is_err());
        Ok(())
    }

    #[test]
    fn test_replace_link() -> Result<()> {
        let (new, links) = replace_links(DATA, LinkType::All);
        assert!(!new.is_empty());
        assert!(!new.contains("![test image](test/test.png)"));
        assert_eq!(links.len(), 3);
        assert!(links.contains(&("1_test.png".to_string(), "test/test.png".parse::<Link>()?)));

        // let (new, links) = replace_links(DATA, LinkType::All);

        Ok(())
    }
}
