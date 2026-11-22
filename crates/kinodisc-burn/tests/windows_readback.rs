//! Reads a real optical (or virtual) drive back against an image, the way
//! the check after a burn does. Manual: mount an ISO (Windows "Mount") or
//! put a burned disc in, then
//!
//! ```text
//! set KINODISC_READBACK_DRIVE=F:
//! set KINODISC_READBACK_ISO=C:\path\disc.iso
//! cargo test -p kinodisc-burn --test windows_readback -- --ignored --nocapture
//! ```
#![cfg(target_os = "windows")]

use kinodisc_burn::{DeviceId, DiscBurner, WindowsBurner};

#[tokio::test]
#[ignore = "needs a drive with the image in it"]
async fn a_drive_reads_back_as_its_image() {
    let drive = std::env::var("KINODISC_READBACK_DRIVE").expect("KINODISC_READBACK_DRIVE");
    let iso = std::env::var("KINODISC_READBACK_ISO").expect("KINODISC_READBACK_ISO");
    let (sender, mut receiver) = tokio::sync::mpsc::channel(64);
    let drain = tokio::spawn(async move { while receiver.recv().await.is_some() {} });
    let report = WindowsBurner::new()
        .verify(&DeviceId::new(drive), std::path::Path::new(&iso), sender)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    drain.await.unwrap();
    println!("{report:?}");
    assert_eq!(report.mismatches, 0, "{report:?}");
}
