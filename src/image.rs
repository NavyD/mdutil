use anyhow::{anyhow, Error, Result};
use futures::AsyncBufReadExt;
use once_cell::sync::Lazy;
use regex::{Captures, Regex};
use reqwest::Client;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::fmt::format;
use std::string::ToString;
use std::time::{Duration, Instant, SystemTime};
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

#[derive(Debug, PartialEq, Eq, Display)]
pub enum Link {
    Local(PathBuf),
    Net(Url),
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
            log::debug!("invalid local path for link: {}, error: {}", link, e);
            Err(anyhow!("link {} is not url or path type", link))
        } else {
            Ok(Link::Local(p.to_path_buf()))
        }
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

pub struct Converter {
    client: Client,
}

impl Converter {
    pub fn new() -> Self {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .build()
            .expect("client build error");
        Self { client }
    }

    pub async fn convert_base64(&self, text: &str, link_type: LinkType) -> Result<String> {
        // 1. get all links with key caps[0]
        let jobs = RE_IMAGE
            .captures_iter(text)
            .map(|caps| {
                (
                    caps[0].to_string(),
                    caps.get(2)
                        .ok_or_else(|| anyhow!("Unable to index 2 in regex: {}", RE_IMAGE.as_str()))
                        .and_then(|s| s.as_str().parse::<Link>()),
                )
            })
            .filter(|(_, link)| link.is_ok())
            .map(|(all, link)| (all, link.unwrap()))
            // 2. filter by link_type and check link
            .filter(|(_, link)| {
                matches!(
                    (link, &link_type),
                    (Link::Local(_), LinkType::All | LinkType::Local)
                        | (Link::Net(_), LinkType::All | LinkType::Net)
                )
            })
            .map(|(all, link)| {
                let client = self.client.clone();
                tokio::spawn(async move {
                    // log::trace!("checking if link {:?} is available", link);
                    if let Err(e) = check_link(&client, &link).await {
                        return Err(anyhow!("found link {} is unavailable: {}", link, e));
                    }
                    // 3. load image in parallel
                    let buf = load_image(&client, &link).await?;
                    // 4. valid magic numbers
                    let fmt = match ImageFormat::try_from(&buf[..]) {
                        Ok(v) => v,
                        Err(e) => {
                            log::warn!("the link: `{:?}` is not an image file", link);
                            return Err(e);
                        }
                    };
                    // to base64 encode bytes
                    Ok::<(String, String, ImageFormat), Error>((all, base64::encode(buf), fmt))
                })
            })
            .collect::<Vec<_>>();

        log::trace!("waiting for {} link tasks", jobs.len());
        let links = futures::future::join_all(jobs)
            .await
            .into_iter()
            .map(|res| res.map_err(Into::into).and_then(|v| v))
            .filter(|res| {
                if let Err(e) = res {
                    log::warn!("a Link task failed: {}", e);
                    false
                } else {
                    true
                }
            })
            .map(Result::unwrap)
            .map(|(key, b64, fmt)| (key, (b64, fmt)))
            .collect::<HashMap<String, (String, ImageFormat)>>();
        log::info!("Complete {} image base64 encoding", links.len());

        // 5. replace links with key val and append base64 to the end
        Ok(replace(text, &links))
    }
}

async fn check_link(client: &Client, link: &Link) -> Result<()> {
    match link {
        Link::Local(p) => {
            let path_str = p.to_str().ok_or_else(|| anyhow!("to str error"))?;
            if !p.is_file() {
                return Err(anyhow!("the link `{}` is not a file", path_str));
            }
            let name = p
                .extension()
                .and_then(|s| s.to_str())
                .ok_or_else(|| anyhow!("to str error"))?;
            if ImageFormat::from_str(name).is_err() {
                return Err(anyhow!("the link {} is not a image", path_str));
            }
        }
        Link::Net(url) => {
            log::trace!("checking if the url `{}` is available", url);
            let resp = client.head(url.as_str()).send().await?;
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

pub async fn check(context: &str, link_type: LinkType) -> Result<()> {
    todo!()
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

    static CLIENT: Lazy<Client> = Lazy::new(|| Client::builder().build().unwrap());

    #[tokio::test]
    async fn convert_image_to_base64() -> Result<()> {
        let converter = Converter::new();
        let old = DATA;
        let new = converter.convert_base64(old, LinkType::All).await?;
        assert!(!new.is_empty());
        assert!(new.len() > old.len());
        assert!(new.lines().count() > old.lines().count());
        assert!(new.contains("# markdown util"));
        assert!(!new.contains("![test image](test/test.png)"));

        assert!(new.contains("![not exist image](test/_not_exists_test.png)"));
        assert!(new.contains("![](https://fsafthe234notexist.com/a/b/c_aaaa/a.png)"));

        let new = converter.convert_base64(old, LinkType::Local).await?;
        assert!(new.contains("![](https://fsafthe234notexist.com/a/b/c_aaaa/a.png)"));
        assert!(new
            .contains("![](https://www.baidu.com/img/PCtm_d9c8750bed0b3c7d089fa7d55720d6cf.png)"));
        assert!(new.contains("![not exist image](test/_not_exists_test.png)"));
        assert!(!new.contains("![test image](test/test.png)"));

        let new = converter.convert_base64(old, LinkType::Net).await?;
        assert!(!new
            .contains("![](https://www.baidu.com/img/PCtm_d9c8750bed0b3c7d089fa7d55720d6cf.png)"));
        Ok(())
    }

    #[tokio::test]
    async fn check_links() -> Result<()> {
        assert!(check_link(
            &CLIENT,
            &Link::Net(
                "https://www.baidu.com/img/PCtm_d9c8750bed0b3c7d089fa7d55720d6cf.png".parse()?
            )
        )
        .await
        .is_ok());
        assert!(check_link(
            &CLIENT,
            &Link::Net("https://fsafthe234notexist.com/a/b/c_aaaa/a.png".parse()?)
        )
        .await
        .is_err());

        assert!(
            check_link(&CLIENT, &Link::Local("test/_not_exists_test.png".parse()?))
                .await
                .is_err()
        );
        assert!(check_link(&CLIENT, &Link::Local("test/test.png".parse()?))
            .await
            .is_ok());
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
        assert!(!load_image(&CLIENT, &Link::Net(link.parse()?))
            .await?
            .is_empty());

        let link = "test/test.png";
        assert!(!load_image(&CLIENT, &Link::Local(link.parse()?))
            .await?
            .is_empty());

        let link = "_not/exist/_file.png";
        assert!(load_image(&CLIENT, &Link::Local(link.parse()?))
            .await
            .is_err());
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
