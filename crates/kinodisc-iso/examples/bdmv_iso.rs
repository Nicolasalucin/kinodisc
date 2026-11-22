//! Developer tool: wraps an existing BDMV folder (BDMV/ and CERTIFICATE/)
//! in the same UDF 2.50 image the render writes.
//! `cargo run -p kinodisc-iso --example bdmv_iso -- <folder> <out.iso> <label>`
use kinodisc_iso::BdmvIsoBuilder;
use std::path::PathBuf;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [folder, iso, label] = args.as_slice() else {
        eprintln!("uso: bdmv_iso <carpeta> <salida.iso> <etiqueta>");
        std::process::exit(2);
    };
    match BdmvIsoBuilder::default()
        .build(&PathBuf::from(folder), &PathBuf::from(iso), label)
        .await
    {
        Ok(report) => println!(
            "{} ({} bytes)",
            report.iso_path.display(),
            report.size_bytes
        ),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
