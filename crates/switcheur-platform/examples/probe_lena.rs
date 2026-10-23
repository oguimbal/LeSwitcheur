//! Talk to a running Lena: `cargo run -p switcheur-platform --example probe_lena -- search devis`
//! (`info`, `search <query>`, `icon <file name>`, `thumbnail <instance> <id>`, `open <instance> <id> [reveal]`, `ask <question>`).
use switcheur_platform::lena;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let started = std::time::Instant::now();
    let result = match args.first().map(String::as_str) {
        Some("search") => {
            lena::search(&args[1..].join(" "), lena::MAX_HITS).map(|(instance, hits)| {
                println!("instance {instance}");
                for hit in hits {
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        hit.id, hit.is_folder, hit.thumbnail, hit.folder, hit.name
                    );
                }
            })
        }
        Some("open") => lena::open(&args[1], &args[2], args.get(3).is_some()),
        Some("icon") => {
            println!("{:?}", switcheur_platform::icon_for_name(std::path::Path::new(&args[1]), false));
            Ok(())
        }
        Some("thumbnail") => lena::thumbnail(&args[1], &args[2]).map(|path| println!("{}", path.display())),
        Some("ask") => lena::ask(&args[1..].join(" ")),
        _ => lena::info().map(|info| println!("{info:?}")),
    };
    if let Err(error) = result {
        println!(
            "error {:?} code {:?}: {error}",
            error.kind(),
            lena::remote_code(&error)
        );
    }
    println!("{:?}", started.elapsed());
}
