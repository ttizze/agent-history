use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug, Clone)]
#[command(name = "agent-history")]
#[command(about = "Codex/Claude の会話履歴を検索するTUI", long_about = None)]
#[command(version)]
pub struct Args {
    /// 追加の検索ルート（再帰）。複数指定可。
    #[arg(long = "root", value_name = "PATH")]
    pub roots: Vec<PathBuf>,

    /// デフォルト検索ルート（~/.codex/sessions と ~/.codex/archived_sessions）を無効化
    #[arg(long)]
    pub no_default_roots: bool,

    /// 追加で ~/.codex/history.jsonl も取り込む（簡易フォーマット）
    #[arg(long = "history")]
    pub include_history: bool,

    /// 起動時クエリ
    #[arg(long, short = 'q', value_name = "QUERY")]
    pub query: Option<String>,

    /// 結果表示の上限（0で無制限）
    #[arg(long, default_value_t = 5000)]
    pub max_results: usize,

    /// TUIを起動せずJSON形式で結果を標準出力に出力する
    #[arg(long)]
    pub json: bool,
}
