#[tokio::main]
async fn main() -> anyhow::Result<()> {
    braindrain_daemon::run_service().await
}
