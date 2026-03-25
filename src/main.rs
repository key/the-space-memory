use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use the_space_memory::cli;
use the_space_memory::user_dict::DictFormat;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum DictFormatArg {
    Simpledic,
    Ipadic,
}

impl From<DictFormatArg> for DictFormat {
    fn from(arg: DictFormatArg) -> Self {
        match arg {
            DictFormatArg::Simpledic => DictFormat::Simpledic,
            DictFormatArg::Ipadic => DictFormat::Ipadic,
        }
    }
}

#[derive(Parser)]
#[command(name = "tsm", version, about = "The Space Memory — knowledge search engine")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize the database
    Init,
    /// Index documents
    Index {
        /// Read file paths from stdin
        #[arg(long)]
        files_from_stdin: bool,
    },
    /// Search documents
    Search {
        /// Search query
        #[arg(short, long)]
        query: String,
        /// Number of results
        #[arg(short = 'k', long, default_value = "5")]
        top_k: usize,
        /// Output format (text or json)
        #[arg(short, long, default_value = "text")]
        format: String,
        /// Include full content for top N results
        #[arg(long)]
        include_content: Option<usize>,
        /// Filter: documents after this date (YYYY-MM-DD, YYYY-MM, or YYYY)
        #[arg(long)]
        after: Option<String>,
        /// Filter: documents before this date (YYYY-MM-DD, YYYY-MM, or YYYY)
        #[arg(long)]
        before: Option<String>,
        /// Filter: documents from the last N days (e.g. "30d", "2w")
        #[arg(long)]
        recent: Option<String>,
        /// Filter: documents from a specific year
        #[arg(long)]
        year: Option<i32>,
    },
    /// Ingest a session JSONL file
    IngestSession {
        /// Path to the JSONL file
        session_file: PathBuf,
    },
    /// Start the embedder daemon
    EmbedderStart,
    /// Download model files from HuggingFace Hub
    Setup,
    /// Fill missing vectors for chunks (needs running embedder)
    VectorFill {
        /// Batch size for processing
        #[arg(long, default_value = "64")]
        batch_size: usize,
    },
    /// Import synonyms from Japanese WordNet
    ImportWordnet {
        /// Path to wnjpn.db
        wordnet_db: PathBuf,
    },
    /// Update user dictionary from collected candidates
    DictUpdate {
        /// Minimum frequency threshold
        #[arg(long, default_value = "5")]
        threshold: i64,
        /// Skip confirmation prompt
        #[arg(long)]
        yes: bool,
        /// CSV format: simpledic (janome) or ipadic (lindera)
        #[arg(long, value_enum, default_value = "ipadic")]
        format: DictFormatArg,
    },
    /// Check system health
    Doctor,
    /// Rebuild database (backup, delete, init, full index)
    Rebuild {
        /// Proceed without confirmation
        #[arg(long)]
        force: bool,
    },
}

fn main() -> anyhow::Result<()> {
    let args = Cli::parse();
    match args.command {
        Commands::Init => cli::cmd_init()?,
        Commands::Index { files_from_stdin } => cli::cmd_index(files_from_stdin)?,
        Commands::Search {
            query,
            top_k,
            format,
            include_content,
            after,
            before,
            recent,
            year,
        } => cli::cmd_search(cli::SearchOptions {
            query: &query,
            top_k,
            format: &format,
            include_content,
            after: after.as_deref(),
            before: before.as_deref(),
            recent: recent.as_deref(),
            year,
        })?,
        Commands::IngestSession { session_file } => cli::cmd_ingest_session(&session_file)?,
        Commands::EmbedderStart => cli::cmd_embedder_start(None)?,
        Commands::Setup => cli::cmd_setup()?,
        Commands::VectorFill { batch_size } => cli::cmd_vector_fill(batch_size)?,
        Commands::ImportWordnet { wordnet_db } => cli::cmd_import_wordnet(&wordnet_db)?,
        Commands::DictUpdate {
            threshold,
            yes,
            format,
        } => cli::cmd_dict_update(threshold, yes, format.into())?,
        Commands::Doctor => cli::cmd_doctor()?,
        Commands::Rebuild { force } => cli::cmd_rebuild(force)?,
    }
    Ok(())
}
