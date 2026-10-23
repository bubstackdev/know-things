use know_things::{contracts::*, store::Store, transport};
use std::{
    io::{Read, Write},
    path::Path,
};
fn main() {
    if let Err(e) = run() {
        let _ = serde_json::to_writer(std::io::stderr(), &e);
        eprintln!();
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|s| s == "restore") && args.len() == 4 {
        let restored = Store::restore(Path::new(&args[2]), Path::new(&args[3]))?;
        serde_json::to_writer_pretty(std::io::stdout(), &restored.integrity()?)?;
        println!();
        return Ok(());
    }
    if args.get(1).is_some_and(|s| s == "assets") && args.len() >= 4 {
        match args[2].as_str() {
            "verify" if args.len() == 4 => {
                know_things::assets::verify(Path::new(&args[3]))?;
                println!("Verified");
                return Ok(());
            }
            "import" if args.len() == 5 => {
                know_things::assets::import(Path::new(&args[3]), Path::new(&args[4]))?;
                println!("Installed");
                return Ok(());
            }
            _ => {
                return Err(Error::new(
                    "invalid_request",
                    "Use assets verify BUNDLE or assets import SOURCE DESTINATION",
                ));
            }
        }
    }
    if args.get(1).is_some_and(|s| s == "tokenizer-probe") && args.len() == 4 {
        let mut worker = know_things::flash::FlashWorker::open(Path::new(&args[2]))?;
        worker.load_tokenizer_bundle(Path::new(&args[3]))?;
        let cases = [
            "Hello, world!",
            "Café 🦀 punctuation redis-cache.DotName",
            "<|im_start|>user\nTest<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n",
            "<think>",
            "</think>",
        ];
        let mut results = Vec::new();
        for text in cases {
            let ids = worker.token_ids(
                text,
                std::time::Instant::now() + std::time::Duration::from_secs(5),
            )?;
            results.push(serde_json::json!({"text":text,"tokenIds":ids}));
        }
        serde_json::to_writer_pretty(std::io::stdout(), &results)?;
        println!();
        return Ok(());
    }
    if args.get(1).is_some_and(|s| s == "flash-probe") && args.len() == 3 {
        let mut worker = know_things::flash::FlashWorker::open(Path::new(&args[2]))?;
        serde_json::to_writer_pretty(std::io::stdout(), &worker.health()?)?;
        println!();
        return Ok(());
    }
    let coordinator = |path: &Path| -> Result<know_things::coordinator::Coordinator> {
        let mut options = std::collections::BTreeMap::new();
        if !(args.len() - 3).is_multiple_of(2) {
            return Err(Error::new("invalid_request", "Options require path values"));
        }
        for pair in args[3..].as_chunks::<2>().0 {
            if !matches!(
                pair[0].as_str(),
                "--embedding-bundle"
                    | "--flash-bundle"
                    | "--generation-profile"
                    | "--generation-startup-deadline-ms"
            ) || options.insert(pair[0].as_str(), pair[1].as_str()).is_some()
            {
                return Err(Error::new("invalid_request", "Unknown or duplicate option"));
            }
        }
        if options.contains_key("--flash-bundle") != options.contains_key("--generation-profile") {
            return Err(Error::new(
                "invalid_request",
                "Generation requires both --flash-bundle and --generation-profile",
            ));
        }
        let generation_startup_ms =
            if let Some(value) = options.get("--generation-startup-deadline-ms") {
                if !options.contains_key("--generation-profile") {
                    return Err(Error::new(
                        "invalid_request",
                        "Generation startup deadline requires a generation profile",
                    ));
                }
                value
                    .parse::<u64>()
                    .ok()
                    .filter(|ms| (1000..=900000).contains(ms))
                    .ok_or_else(|| {
                        Error::new(
                            "invalid_request",
                            "Generation startup deadline must be 1000–900000 ms",
                        )
                    })?
            } else {
                120000
            };
        let mut store = Store::open(path, None)?;
        let mut embedding_error = None;
        if let Some(root) = options.get("--embedding-bundle") {
            match know_things::embedding::LocalEmbedder::open(Path::new(root)) {
                Ok(embedder) => store.attach_embedder(Box::new(embedder))?,
                Err(error) => {
                    eprintln!("{}", serde_json::to_string(&error)?);
                    embedding_error = Some(error);
                }
            }
        }
        let mut generation_error = None;
        let generator: Option<Box<dyn know_things::generation::Generator>> =
            if let (Some(worker), Some(profile)) = (
                options.get("--flash-bundle"),
                options.get("--generation-profile"),
            ) {
                match know_things::generation::NativeGenerator::open(
                    Path::new(worker),
                    Path::new(profile),
                    std::time::Instant::now()
                        + std::time::Duration::from_millis(generation_startup_ms),
                ) {
                    Ok(generator) => Some(Box::new(generator)),
                    Err(error) => {
                        eprintln!("{}", serde_json::to_string(&error)?);
                        generation_error = Some(error);
                        None
                    }
                }
            } else {
                None
            };
        Ok(know_things::coordinator::Coordinator::with_model_errors(
            store,
            generator,
            generation_error,
            embedding_error,
        ))
    };
    match args.get(1).map(String::as_str) {
        Some("serve") if args.len() >= 3 => {
            let path = Path::new(&args[2]);
            transport::serve_coordinator(path, coordinator(path)?)
        }
        Some("local") if args.len() >= 3 => {
            let path = Path::new(&args[2]);
            transport::local_coordinator(
                coordinator(path)?,
                std::io::stdin(),
                transport::DeadlineStdout::new()?,
            )
        }
        Some("mcp-stdio") if args.len() >= 3 => transport::mcp_coordinator(
            coordinator(Path::new(&args[2]))?,
            std::io::stdin(),
            transport::DeadlineStdout::new()?,
        ),
        Some("request") if args.len() == 3 => {
            let mut bytes = Vec::new();
            std::io::stdin()
                .take((transport::MAX_FRAME + 1) as u64)
                .read_to_end(&mut bytes)?;
            if bytes.len() > transport::MAX_FRAME {
                return Err(Error::new("resource_exhausted", "Request exceeds 1 MiB"));
            }
            let request: Request = serde_json::from_slice(&bytes)?;
            let mut failed = false;
            transport::call_stream(Path::new(&args[2]), &request, |response| {
                failed = response.error.is_some();
                let mut bytes = serde_json::to_vec(&response)?;
                bytes.push(b'\n');
                std::io::stdout().write_all(&bytes)?;
                std::io::stdout().flush()?;
                Ok(())
            })?;
            if failed {
                std::process::exit(2);
            }
            Ok(())
        }
        Some("mcp") if args.len() == 3 => transport::mcp(
            Path::new(&args[2]),
            std::io::stdin(),
            transport::DeadlineStdout::new()?,
        ),
        Some("schema") => {
            serde_json::to_writer_pretty(std::io::stdout(), &transport::operation_schema())?;
            Ok(())
        }
        _ => {
            println!(
                "Know-Things {}\n\n  know-things serve DATA_DIR [ASSET_OPTIONS]\n  know-things local DATA_DIR [ASSET_OPTIONS] < requests.ndjson > responses.ndjson\n  know-things mcp-stdio DATA_DIR [ASSET_OPTIONS]\n  know-things request DATA_DIR < request.json\n  know-things mcp DATA_DIR\n  know-things schema\n\nASSET_OPTIONS: --embedding-bundle PATH; --flash-bundle PATH --generation-profile PATH\nRequests require schemaVersion=1, requestId, and operation. See README.md.",
                env!("CARGO_PKG_VERSION")
            );
            Ok(())
        }
    }
}
