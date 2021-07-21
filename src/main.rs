use anyhow::Result;
use mdutil::cmd;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    cmd::run().await
}
