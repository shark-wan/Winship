use std::process::ExitCode;
use winship::{app::WinshipApp, privileged};

fn main() -> ExitCode {
    let arguments = std::env::args().collect::<Vec<_>>();
    if arguments
        .get(1)
        .is_some_and(|argument| argument == "privileged")
    {
        return run_privileged(&arguments[1..]);
    }

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 860.0])
            .with_min_inner_size([960.0, 680.0]),
        ..Default::default()
    };
    match eframe::run_native(
        "Winship",
        options,
        Box::new(|context| Ok(Box::new(WinshipApp::new(context)))),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Could not start Winship: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_privileged(arguments: &[String]) -> ExitCode {
    match arguments.get(1).map(String::as_str) {
        Some("apply-vfio") if arguments.len() == 3 => {
            let Some(ids) = privileged::parse_ids_argument(&arguments[2]) else {
                eprintln!("Invalid VFIO device ID list");
                return ExitCode::FAILURE;
            };
            match privileged::apply_vfio(&ids) {
                Ok(()) => {
                    println!(
                        "VFIO config written; initramfs updated. Reboot to apply device binding."
                    );
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("{error}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("import-media") if arguments.len() == 4 => {
            match privileged::import_media(
                std::path::Path::new(&arguments[2]),
                std::path::Path::new(&arguments[3]),
            ) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("{error}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("create-disk") if arguments.len() == 4 => {
            let Ok(size_gib) = arguments[2].parse::<u16>() else {
                eprintln!("Disk size must be between 16 and 4096 GiB");
                return ExitCode::FAILURE;
            };
            match privileged::create_disk(std::path::Path::new(&arguments[3]), size_gib) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("{error}");
                    ExitCode::FAILURE
                }
            }
        }
        _ => {
            eprintln!(
                "Usage: winship privileged apply-vfio <vendor:device[,vendor:device...]> | create-disk <GiB> <path> | import-media <source> <destination>"
            );
            ExitCode::FAILURE
        }
    }
}
