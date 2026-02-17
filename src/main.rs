mod args;
mod headless;
mod indexer;
mod search;
mod tui;

use anyhow::Context as _;
use clap::Parser;

fn main() -> anyhow::Result<()> {
    let args = args::Args::parse();
    if args.json {
        headless::run(args).context("JSON出力に失敗しました")?;
    } else {
        tui::run(args).context("TUIの実行に失敗しました")?;
    }
    Ok(())
}
