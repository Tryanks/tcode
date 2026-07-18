fn main() {
    if std::env::args()
        .skip(1)
        .any(|argument| argument == "--permissions")
    {
        let status = computer_use_mcp::permissions::check();
        println!(
            "{}",
            serde_json::to_string(&status).expect("permission status must serialize")
        );
        return;
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create smoke-test runtime");
    let run = runtime.block_on(computer_use_mcp::tools::run_smoke());
    println!(
        "{}",
        serde_json::to_string(&run.verdict).expect("smoke verdict must serialize")
    );
    std::process::exit(run.exit_code);
}
