use std::path::PathBuf;

fn print_json(value: &impl serde::Serialize) {
    println!(
        "{}",
        serde_json::to_string(value).expect("debug tool result must serialize")
    );
}

fn take_argument(arguments: &mut impl Iterator<Item = String>, flag: &str) -> String {
    arguments
        .next()
        .unwrap_or_else(|| panic!("{flag} requires an argument"))
}

fn main() {
    let mut arguments = std::env::args().skip(1).peekable();
    if arguments
        .peek()
        .is_some_and(|argument| argument == "--permissions")
    {
        let status = computer_use_mcp::permissions::check();
        print_json(&status);
        return;
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create smoke-test runtime");

    if arguments
        .peek()
        .is_some_and(|argument| argument.starts_with("--"))
    {
        let result = runtime.block_on(async {
            let mut last_root: Option<String> = None;
            while let Some(flag) = arguments.next() {
                match flag.as_str() {
                    "--find-roots" => {
                        let filter = arguments
                            .next_if(|argument| !argument.starts_with("--"))
                            .unwrap_or_else(|| "{}".into());
                        let result = computer_use_mcp::tools::debug_find_roots(&filter)
                            .await
                            .map_err(|error| error.to_string())?;
                        print_json(&result);
                    }
                    "--observe" => {
                        let root = take_argument(&mut arguments, "--observe");
                        last_root = (!root.is_empty()).then_some(root.clone());
                        print_json(&computer_use_mcp::tools::debug_observe(&root).await);
                    }
                    "--search" => {
                        let state_id = take_argument(&mut arguments, "--search");
                        let text = take_argument(&mut arguments, "--search");
                        print_json(&computer_use_mcp::tools::debug_search(&state_id, &text).await);
                    }
                    "--act" => {
                        let params = take_argument(&mut arguments, "--act");
                        let result = computer_use_mcp::tools::debug_act(&params)
                            .await
                            .map_err(|error| error.to_string())?;
                        print_json(&result);
                    }
                    "--screenshot" => {
                        let output = PathBuf::from(take_argument(&mut arguments, "--screenshot"));
                        let screenshot =
                            computer_use_mcp::tools::debug_screenshot(last_root.as_deref()).await;
                        if let Some(png) = screenshot.png {
                            std::fs::write(&output, png).map_err(|error| error.to_string())?;
                        }
                        print_json(&serde_json::json!({
                            "output": output,
                            "tool_result": screenshot.result,
                        }));
                    }
                    unknown => return Err(format!("unknown debug subcommand: {unknown}")),
                }
            }
            Ok::<(), String>(())
        });
        if let Err(error) = result {
            print_json(&serde_json::json!({ "error": error }));
            std::process::exit(2);
        }
        return;
    }

    let run = runtime.block_on(computer_use_mcp::tools::run_smoke());
    print_json(&run.verdict);
    std::process::exit(run.exit_code);
}
