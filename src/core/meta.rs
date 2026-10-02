use crate::bornes;
use crate::core::store;
use std::process::ExitCode;

/// Schliffe's own meta-commands — invoked as `schliffe <something>` for real,
/// not as a shim (`argv[0]` literally "schliffe", no symlink/copy involved).
/// Routes to the `borne` that owns each feature: `schliffe mcp` goes to
/// `bornes::mcp` (specs §6), `schliffe compress` uses `bornes::prosa`
/// (specs §7.2), `schliffe show`/`schliffe store` use the shared store
/// (`core::store`, specs §8).
pub fn run(args: &[String]) -> ExitCode {
    // `schliffe mcp -- <real command> [args...]` — separated from the other
    // meta-commands because it has variable arity (everything after "--"
    // belongs to the real server, not to Schliffe).
    if args.first().map(String::as_str) == Some("mcp") {
        let options: Vec<&String> = args
            .iter()
            .skip(1)
            .take_while(|a| a.as_str() != "--")
            .collect();
        if let Some(url) = options
            .windows(2)
            .find(|pair| pair[0] == "--url")
            .map(|pair| pair[1].clone())
        {
            let headers = options
                .windows(2)
                .filter(|pair| pair[0] == "--header")
                .filter_map(|pair| pair[1].split_once('='))
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect();
            return bornes::mcp::run_http_with_options(&url, headers, parse_mcp_options(&options));
        }
        let after_sep = args
            .iter()
            .skip(1)
            .skip_while(|a| a.as_str() != "--")
            .skip(1);
        let server_args: Vec<String> = after_sep.cloned().collect();
        // Options live between "mcp" and "--".
        return match server_args.split_first() {
            Some((cmd, rest)) => {
                bornes::mcp::run_with_options(cmd, rest, parse_mcp_options(&options))
            }
            None => {
                eprintln!(
                    "usage: schliffe mcp [options] -- <real MCP server command> [args...]\n       schliffe mcp [options] --url <https://server/mcp>\noptions: --keep-schemas --oauth --compression low|medium|high|max --include-tools a,b --exclude-tools a,b --header Name=Value"
                );
                ExitCode::FAILURE
            }
        };
    }

    match args {
        [cmd, hash] if cmd == "show" => match store::get(hash) {
            Some(content) => {
                print!("{content}");
                if !content.ends_with('\n') {
                    println!();
                }
                ExitCode::SUCCESS
            }
            None => {
                eprintln!("schliffe: hash '{hash}' not found in the store");
                ExitCode::FAILURE
            }
        },
        [cmd, sub] if cmd == "store" && sub == "clear" => match store::clear_all() {
            Ok(()) => {
                println!("schliffe: store cleared");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("schliffe: failed to clear the store: {e}");
                ExitCode::FAILURE
            }
        },
        [cmd] if cmd == "--version" || cmd == "version" => {
            println!("schliffe {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        [cmd, sub] if cmd == "hook" && sub == "post-tool-use" => bornes::hook::run_post_tool_use(),
        [cmd, sub] if cmd == "hook" && sub == "user-prompt-submit" => {
            bornes::prompt::run_user_prompt_submit()
        }
        [cmd, sub] if cmd == "hook" && sub == "install" => bornes::hook::install(),
        [cmd, sub] if cmd == "hook" && sub == "uninstall" => bornes::hook::uninstall(),
        [cmd] if cmd == "report" => {
            print!("{}", crate::core::report::run(7));
            ExitCode::SUCCESS
        }
        [cmd, flag, n] if cmd == "report" && flag == "--days" => match n.parse::<u64>() {
            Ok(days) if days > 0 => {
                print!("{}", crate::core::report::run(days));
                ExitCode::SUCCESS
            }
            _ => {
                eprintln!("schliffe: '--days' needs a positive number, got '{n}'");
                ExitCode::FAILURE
            }
        },
        [cmd] if cmd == "stats" => {
            print!("{}", crate::core::stats::report());
            ExitCode::SUCCESS
        }
        [cmd, sub] if cmd == "store" && sub == "gc" => {
            store::force_gc();
            println!("schliffe: cleanup sweep completed");
            ExitCode::SUCCESS
        }
        // `schliffe compress [--sentences N]` (specs.md §7.2) — standalone
        // utility for `bornes/prosa`: reads all of stdin, summarizes, prints.
        // Only meant for prose that can tolerate losing a whole sentence
        // (commit body, narrative text) — NOT used by the user's
        // `/compress` (specs §7.3 revised: a prompt draft needs sentence-by-
        // sentence semantic judgment, not whole-sentence selection).
        [cmd] if cmd == "compress" => run_compress(None),
        [cmd, flag, n] if cmd == "compress" && flag == "--sentences" => match n.parse::<usize>() {
            Ok(n) => run_compress(Some(n)),
            Err(_) => {
                eprintln!("schliffe: '--sentences' needs a number, got '{n}'");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!(
                "usage: schliffe --version | schliffe stats | schliffe report [--days N] | schliffe hook install|uninstall | schliffe show <hash> | schliffe store clear | schliffe store gc | schliffe compress [--sentences N]"
            );
            ExitCode::FAILURE
        }
    }
}

fn parse_mcp_options(args: &[&String]) -> bornes::mcp::Options {
    let mut options = bornes::mcp::Options {
        lazy_schemas: !args.iter().any(|arg| *arg == "--keep-schemas"),
        ..bornes::mcp::Options::default()
    };
    if let Some(pair) = args.windows(2).find(|pair| pair[0] == "--compression") {
        options.compression = match pair[1].as_str() {
            "low" => bornes::mcp::CompressionLevel::Low,
            "high" => bornes::mcp::CompressionLevel::High,
            "max" => bornes::mcp::CompressionLevel::Max,
            _ => bornes::mcp::CompressionLevel::Medium,
        };
    }
    options.include_tools = csv_option(args, "--include-tools");
    options.exclude_tools = csv_option(args, "--exclude-tools");
    options.oauth = args.iter().any(|arg| arg.as_str() == "--oauth");
    options
}

fn csv_option(args: &[&String], flag: &str) -> Vec<String> {
    args.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| {
            pair[1]
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn run_compress(max_sentences: Option<usize>) -> ExitCode {
    use std::io::Read;
    let mut text = String::new();
    if std::io::stdin().read_to_string(&mut text).is_err() {
        eprintln!("schliffe: failed to read stdin");
        return ExitCode::FAILURE;
    }
    // Found while testing the cross-compiled .exe on native PowerShell (M8,
    // 2026-07-26): `"text" | schliffe.exe compress` arrives with a UTF-8 BOM
    // (U+FEFF) at the front — a known behavior of how native PowerShell
    // encodes a string literal when piping it to a process's stdin, not an
    // schliffe bug. A BOM carries no meaning in plain text, so stripping it
    // doesn't risk business rule 5 (nothing substantive is lost).
    if let Some(rest) = text.strip_prefix('\u{feff}') {
        text = rest.to_string();
    }
    let budget = max_sentences.unwrap_or_else(|| bornes::prosa::suggested_sentence_budget(&text));
    let out = bornes::prosa::summarize(&text, budget);
    println!("{out}");
    ExitCode::SUCCESS
}
