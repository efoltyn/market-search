#[tokio::main]
async fn main() {
    if let Err(err) = eli_cli::run().await {
        if let Some(clap_err) = err.downcast_ref::<clap::Error>() {
            let _ = clap_err.print();
            let kind = clap_err.kind();
            if matches!(
                kind,
                clap::error::ErrorKind::UnknownArgument
                    | clap::error::ErrorKind::InvalidValue
                    | clap::error::ErrorKind::ValueValidation
            ) {
                eprintln!(
                    "[ERROR: Invalid flag. RECOVERY: Do not invent flags. Refer to the Data Dictionary. Tickers like GC=F or ^TNX are passed directly to --tickers.]"
                );
            }
            std::process::exit(clap_err.exit_code());
        } else {
            eprintln!("{err:?}");
            std::process::exit(1);
        }
    }
}
