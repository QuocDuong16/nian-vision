use std::path::PathBuf;

fn main() {
    let mut args = std::env::args_os().skip(1);
    let Some(artifact) = args.next() else {
        eprintln!("usage: nian-release-verifier <AppImage> <AppImage.sig>");
        std::process::exit(2);
    };
    let Some(signature) = args.next() else {
        eprintln!("usage: nian-release-verifier <AppImage> <AppImage.sig>");
        std::process::exit(2);
    };
    if args.next().is_some() {
        eprintln!("usage: nian-release-verifier <AppImage> <AppImage.sig>");
        std::process::exit(2);
    }

    let public_key = match std::env::var("NIAN_UPDATER_PUBLIC_KEY") {
        Ok(value) if !value.trim().is_empty() => value,
        _ => {
            eprintln!("nian-release-verifier: NIAN_UPDATER_PUBLIC_KEY is required");
            std::process::exit(2);
        }
    };

    let artifact = PathBuf::from(artifact);
    let signature = PathBuf::from(signature);
    if let Err(error) = nian_release_verifier::verify_file(&artifact, &signature, &public_key) {
        eprintln!("nian-release-verifier: {error}");
        std::process::exit(1);
    }

    println!("updater signature verified for {}", artifact.display());
}
