fn main() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let client = reqwest::Client::builder().user_agent("pawchive-downloader/0.3.0").build()?;
        let r = client.get("https://pawchive.pw/api/posts?p=1").send().await?;
        let status = r.status();
        let body = r.text().await?;
        println!("STATUS {status} LEN {}", body.len());
        println!("HEAD {}", &body[..body.len().min(120)]);
        Ok::<_, anyhow::Error>(())
    }).unwrap();
}
